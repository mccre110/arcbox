//! USB accessory manager owned by the [`Runtime`](crate::runtime::Runtime).
//!
//! The macOS implementation registers the Accessory Access listener once at
//! daemon startup and drives attach/detach against the System VM. On other
//! platforms every operation degrades gracefully (empty listing, clear
//! errors) so callers need no platform branching.

use crate::error::Result;
use crate::event::EventBus;
use crate::machine::MachineManager;
use std::sync::Arc;

use super::{UsbDeviceSnapshot, UsbSelector};

#[cfg(target_os = "macos")]
use crate::error::CoreError;
#[cfg(target_os = "macos")]
use crate::event::Event;
#[cfg(target_os = "macos")]
use crate::vm_lifecycle::DEFAULT_MACHINE_NAME;
#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
type Registry =
    super::registry::UsbRegistry<Arc<arcbox_vz::UsbAccessory>, Arc<arcbox_vz::UsbDevice>>;

/// Manages host USB accessories granted to the daemon and their attachment
/// to the System VM.
///
/// Attachments are runtime-only: a System VM stop (observed via the event
/// bus) resets every entry to available.
pub struct UsbManager {
    #[cfg(target_os = "macos")]
    machine_manager: Arc<MachineManager>,
    #[cfg(target_os = "macos")]
    registry: Arc<Mutex<Registry>>,
    /// Whether the accessory listener registered successfully. False on
    /// macOS < 27, without the macOS 27 SDK, or when registration was
    /// rejected (e.g. missing entitlement) — attach/detach then fail with
    /// a clear precondition error instead of a puzzling "not found".
    #[cfg(target_os = "macos")]
    supported: AtomicBool,
}

#[cfg(target_os = "macos")]
impl UsbManager {
    /// Creates the manager. Call [`start`](Self::start) once from an async
    /// context to begin receiving accessory events.
    #[must_use]
    pub fn new(machine_manager: Arc<MachineManager>) -> Self {
        Self {
            machine_manager,
            registry: Arc::new(Mutex::new(Registry::new())),
            supported: AtomicBool::new(false),
        }
    }

    /// Registers the process-lifetime accessory listener and spawns the
    /// event pump. Call at most once, at daemon startup.
    ///
    /// Degrades gracefully: when USB passthrough is unsupported on this
    /// host, or registration is rejected (e.g. missing the USB accessory
    /// entitlement), this logs and returns — listing stays empty.
    ///
    /// The listener is registered whenever the host supports it, regardless
    /// of the active VM backend: grants collected while on HV become usable
    /// after a switch to VZ without a daemon restart (attach on HV fails
    /// with a clear error).
    pub async fn start(&self, event_bus: &EventBus) {
        if !arcbox_vz::usb_passthrough_supported() {
            tracing::debug!("USB passthrough not supported on this host; accessory listener off");
            return;
        }

        let mut events = match arcbox_vz::register_accessory_listener().await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("USB accessory listener registration failed: {e}");
                return;
            }
        };
        self.supported.store(true, Ordering::Relaxed);
        tracing::info!("USB accessory listener registered");

        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Ok(mut registry) = registry.lock() else {
                    tracing::error!("USB registry lock poisoned; accessory events dropped");
                    return;
                };
                match event {
                    arcbox_vz::UsbAccessoryEvent::Connected(accessory) => {
                        let info = super::UsbDeviceInfo::from(accessory.info().clone());
                        tracing::info!(
                            registry_id = info.registry_id,
                            vendor_id = format!("{:04x}", info.vendor_id),
                            product_id = format!("{:04x}", info.product_id),
                            name = info.name.as_deref().unwrap_or(""),
                            "USB accessory connected"
                        );
                        registry.connect(info, Arc::new(accessory));
                    }
                    arcbox_vz::UsbAccessoryEvent::Disconnected(info) => {
                        // The entry (with any attached-device handle) drops
                        // here; the framework already removed the device
                        // from the VM along with the physical accessory.
                        let was_attached =
                            registry.disconnect(info.registry_id).is_some_and(|entry| {
                                matches!(entry.state, super::registry::AttachState::Attached(_))
                            });
                        tracing::info!(
                            registry_id = info.registry_id,
                            was_attached,
                            "USB accessory disconnected"
                        );
                    }
                }
            }
        });

        // Attachments die with the VM: reset the bookkeeping on every
        // System VM stop so stale device handles are dropped.
        let registry = Arc::clone(&self.registry);
        let mut bus_events = event_bus.subscribe();
        tokio::spawn(async move {
            loop {
                match bus_events.recv().await {
                    Ok(Event::MachineStopped { name }) if name == DEFAULT_MACHINE_NAME => {
                        if let Ok(mut registry) = registry.lock() {
                            registry.reset_attachments();
                            tracing::debug!("System VM stopped; USB attachments reset");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Lists granted accessories and their attachment state.
    ///
    /// Guest paths are not filled in here — see
    /// [`annotate_guest_paths`](super::annotate_guest_paths).
    #[must_use]
    pub fn list(&self) -> Vec<UsbDeviceSnapshot> {
        self.registry
            .lock()
            .map_or_else(|_| Vec::new(), |r| r.list())
    }

    /// Attaches the selected accessory to the running System VM (hot-plug).
    ///
    /// # Errors
    ///
    /// Returns an error when the selector does not resolve to exactly one
    /// granted accessory, the accessory is already attached, the System VM
    /// is not running on the VZ backend, or the framework rejects the
    /// attach.
    pub async fn attach(&self, selector: &UsbSelector) -> Result<()> {
        self.ensure_supported()?;
        let (registry_id, accessory) = self
            .registry
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?
            .begin_attach(selector)?;

        // The VZ attach blocks up to its completion timeout — keep it off
        // the async executor.
        let machine_manager = Arc::clone(&self.machine_manager);
        let result = tokio::task::spawn_blocking(move || {
            machine_manager.attach_usb(DEFAULT_MACHINE_NAME, &accessory)
        })
        .await
        .map_err(|e| CoreError::Vm(format!("USB attach task panicked: {e}")))
        .and_then(|result| result);

        let mut registry = self.registry.lock().map_err(|_| CoreError::LockPoisoned)?;
        match result {
            Ok(device) => {
                registry.complete_attach(registry_id, Arc::new(device));
                Ok(())
            }
            Err(e) => {
                registry.abort_attach(registry_id);
                Err(e)
            }
        }
    }

    /// Detaches the selected accessory from the running System VM.
    ///
    /// # Errors
    ///
    /// Returns an error when the selector does not resolve, the accessory
    /// is not attached, or the framework rejects the detach (the entry then
    /// stays attached).
    pub async fn detach(&self, selector: &UsbSelector) -> Result<()> {
        self.ensure_supported()?;
        let (registry_id, device) = self
            .registry
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?
            .begin_detach(selector)?;

        let machine_manager = Arc::clone(&self.machine_manager);
        let result = tokio::task::spawn_blocking(move || {
            machine_manager.detach_usb(DEFAULT_MACHINE_NAME, &device)
        })
        .await
        .map_err(|e| CoreError::Vm(format!("USB detach task panicked: {e}")))
        .and_then(|result| result);

        result?;
        self.registry
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?
            .complete_detach(registry_id);
        Ok(())
    }

    /// Rejects attach/detach with a clear precondition error when the
    /// accessory listener never registered.
    fn ensure_supported(&self) -> Result<()> {
        if self.supported.load(Ordering::Relaxed) {
            return Ok(());
        }
        Err(CoreError::invalid_state(
            "USB passthrough requires macOS 27 or later with Accessory Access (and the USB \
             accessory entitlement); the accessory listener is not registered",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
impl UsbManager {
    /// Creates the manager (non-macOS stub — no accessory source exists).
    #[must_use]
    pub fn new(_machine_manager: Arc<MachineManager>) -> Self {
        Self {}
    }

    /// No-op: USB passthrough requires macOS.
    pub async fn start(&self, _event_bus: &EventBus) {}

    /// Always empty: no accessory source on this platform.
    #[must_use]
    pub fn list(&self) -> Vec<UsbDeviceSnapshot> {
        Vec::new()
    }

    /// Always fails: USB passthrough requires macOS.
    ///
    /// # Errors
    ///
    /// Always returns an invalid-state error.
    pub async fn attach(&self, _selector: &UsbSelector) -> Result<()> {
        Err(Self::unsupported())
    }

    /// Always fails: USB passthrough requires macOS.
    ///
    /// # Errors
    ///
    /// Always returns an invalid-state error.
    pub async fn detach(&self, _selector: &UsbSelector) -> Result<()> {
        Err(Self::unsupported())
    }

    fn unsupported() -> crate::error::CoreError {
        crate::error::CoreError::invalid_state(
            "USB passthrough requires macOS 27 or later with the VZ backend",
        )
    }
}
