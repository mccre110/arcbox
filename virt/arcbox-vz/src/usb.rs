//! USB passthrough via the macOS 27 Accessory Access framework.
//!
//! The user grants USB accessories to the application through the system
//! Accessory Access UI. A process-lifetime listener (registered once via
//! [`register_accessory_listener`]) delivers connect/disconnect events;
//! granted accessories are hot-plugged into a running VM through
//! [`UsbController::attach`] and removed with [`UsbController::detach`].
//!
//! Requires macOS 27+ and a binary built against the macOS 27 SDK; every
//! entry point returns [`VZError::NotSupported`]-style errors otherwise
//! (probe with [`crate::usb_passthrough_supported`]).

use crate::error::{VZError, VZResult};
use crate::restore::object_trampoline;
use crate::shim_ffi;
use crate::vm::state_trampoline;
use std::ffi::{c_char, c_void};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Identity of a USB accessory as reported by the host.
#[derive(Debug, Clone)]
pub struct UsbAccessoryInfo {
    /// IORegistry entry ID — stable for the duration of a connection.
    pub registry_id: u64,
    /// USB vendor ID (`idVendor`).
    pub vendor_id: u16,
    /// USB product ID (`idProduct`).
    pub product_id: u16,
    /// Product name from the IORegistry, when available.
    pub name: Option<String>,
    /// Serial number from the IORegistry, when available.
    pub serial: Option<String>,
}

/// A USB accessory granted to this process, attachable to a VM.
pub struct UsbAccessory {
    /// +1 AAUSBAccessory handle.
    handle: *mut c_void,
    info: UsbAccessoryInfo,
}

// SAFETY: The handle refers to an immutable, Sendable AAUSBAccessory; all
// access goes through the shim.
unsafe impl Send for UsbAccessory {}
// SAFETY: See above — the accessory object is immutable.
unsafe impl Sync for UsbAccessory {}

impl UsbAccessory {
    /// The accessory's identity.
    #[must_use]
    pub fn info(&self) -> &UsbAccessoryInfo {
        &self.info
    }
}

impl Drop for UsbAccessory {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: releasing the +1 handle delivered by the event callback.
            unsafe { shim_ffi::abx_object_release(self.handle) };
        }
    }
}

/// An accessory connect/disconnect event from the host.
pub enum UsbAccessoryEvent {
    /// The user attached an accessory to this application (or it
    /// reconnected). The accessory can be attached to a VM.
    Connected(UsbAccessory),
    /// The accessory was unplugged or the user detached it from this
    /// application.
    Disconnected(UsbAccessoryInfo),
}

/// Recurring event trampoline: borrows the leaked process-lifetime sender.
unsafe extern "C" fn usb_event_trampoline(
    ctx: *mut c_void,
    accessory: *mut c_void,
    registry_id: u64,
    vendor_id: u16,
    product_id: u16,
    name: *mut c_char,
    serial: *mut c_char,
    connected: bool,
) {
    // SAFETY: ctx is the leaked Box<UnboundedSender> from
    // register_accessory_listener — valid for the process lifetime; the
    // recurring callback contract means it is never freed. Strings are null
    // or shim strdup's that take_string frees.
    unsafe {
        let sender = &*ctx.cast::<mpsc::UnboundedSender<UsbAccessoryEvent>>();
        let info = UsbAccessoryInfo {
            registry_id,
            vendor_id,
            product_id,
            name: shim_ffi::take_string(name),
            serial: shim_ffi::take_string(serial),
        };
        let event = if connected {
            UsbAccessoryEvent::Connected(UsbAccessory {
                handle: accessory,
                info,
            })
        } else {
            UsbAccessoryEvent::Disconnected(info)
        };
        // If the receiver is gone the event drops here and UsbAccessory's
        // Drop releases the +1 accessory handle.
        let _ = sender.send(event);
    }
}

/// Registers the process-lifetime USB accessory listener.
///
/// Accessories already granted to the application are replayed as
/// [`UsbAccessoryEvent::Connected`] events before this returns. Call at most
/// once per process — the event channel sender is intentionally leaked
/// because the host keeps delivering events for the process lifetime.
///
/// # Errors
///
/// Returns an error when USB passthrough is unsupported on this host or the
/// registration is rejected (e.g. missing the USB accessory entitlement).
pub async fn register_accessory_listener() -> VZResult<mpsc::UnboundedReceiver<UsbAccessoryEvent>> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    // Leaked on purpose: the recurring callback references it forever.
    let event_ctx: *mut c_void = Box::into_raw(Box::new(event_tx)).cast();

    let (tx, rx) = oneshot::channel::<Result<(), String>>();
    let ctx: *mut c_void = Box::into_raw(Box::new(tx)).cast();
    // SAFETY: event_ctx ownership transfers to the recurring event callback
    // (process-lifetime); ctx ownership transfers to the exactly-once
    // completion trampoline.
    unsafe {
        shim_ffi::abx_usb_manager_register(event_ctx, usb_event_trampoline, ctx, state_trampoline);
    }

    let result = rx.await.map_err(|_| VZError::Internal {
        code: -1,
        message: "USB listener registration cancelled".into(),
    })?;
    result.map_err(|msg| {
        VZError::OperationFailed(format!("USB listener registration failed: {msg}"))
    })?;
    Ok(event_rx)
}

/// A runtime USB controller on a running VM.
///
/// Every operation dispatches onto the VM's serial queue inside the shim
/// (VZ device objects are queue-affine).
pub struct UsbController {
    /// +1 ABXUsbControllerBox handle pairing the controller with the VM's
    /// queue.
    controller_box: *mut c_void,
}

// SAFETY: The box handle is only used through the shim, which serializes
// every device access on the VM's queue as the framework requires.
unsafe impl Send for UsbController {}
// SAFETY: See above — every method dispatches onto the VM's serial queue.
unsafe impl Sync for UsbController {}

impl UsbController {
    /// Wraps a +1 `ABXUsbControllerBox` handle produced by the shim.
    pub(crate) fn from_box(controller_box: *mut c_void) -> Self {
        Self { controller_box }
    }

    /// Attaches a granted accessory to this controller (hot-plug).
    ///
    /// Returns the attached device, needed later for [`detach`](Self::detach).
    ///
    /// # Errors
    ///
    /// Returns an error when the framework rejects the attach (unsupported
    /// device type, accessory revoked, VM not running, ...).
    pub async fn attach(&self, accessory: &UsbAccessory) -> VZResult<UsbDevice> {
        let (tx, rx) = oneshot::channel::<Result<usize, String>>();
        let ctx: *mut c_void = Box::into_raw(Box::new(tx)).cast();
        // SAFETY: both handles are valid; ctx ownership transfers to the
        // exactly-once trampoline, which releases the +1 device handle if
        // the receiver is gone.
        unsafe {
            shim_ffi::abx_usb_controller_attach(
                self.controller_box,
                accessory.handle,
                ctx,
                object_trampoline,
            );
        }

        let bits = rx
            .await
            .map_err(|_| VZError::Internal {
                code: -1,
                message: "USB attach cancelled".into(),
            })?
            .map_err(|msg| VZError::OperationFailed(format!("USB attach failed: {msg}")))?;
        Ok(UsbDevice {
            device_box: bits as *mut c_void,
        })
    }

    /// Detaches a previously attached device (hot-unplug).
    ///
    /// # Errors
    ///
    /// Returns an error when the framework rejects the detach.
    pub async fn detach(&self, device: &UsbDevice) -> VZResult<()> {
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        let ctx: *mut c_void = Box::into_raw(Box::new(tx)).cast();
        // SAFETY: both handles are valid; ctx ownership transfers to the
        // exactly-once trampoline.
        unsafe {
            shim_ffi::abx_usb_controller_detach(
                self.controller_box,
                device.device_box,
                ctx,
                state_trampoline,
            );
        }

        let result = rx.await.map_err(|_| VZError::Internal {
            code: -1,
            message: "USB detach cancelled".into(),
        })?;
        result.map_err(|msg| VZError::OperationFailed(format!("USB detach failed: {msg}")))
    }
}

impl Drop for UsbController {
    fn drop(&mut self) {
        if !self.controller_box.is_null() {
            // SAFETY: releasing the +1 box handle returned by the shim.
            unsafe { shim_ffi::abx_object_release(self.controller_box) };
        }
    }
}

/// A USB device attached to a running VM, used to detach it later.
pub struct UsbDevice {
    /// +1 ABXUsbDeviceBox handle pairing the device with the VM's queue.
    device_box: *mut c_void,
}

// SAFETY: The box handle is only used through the shim, which serializes
// every device access on the VM's queue as the framework requires.
unsafe impl Send for UsbDevice {}
// SAFETY: See above.
unsafe impl Sync for UsbDevice {}

impl Drop for UsbDevice {
    fn drop(&mut self) {
        if !self.device_box.is_null() {
            // SAFETY: releasing the +1 box handle returned by the shim.
            unsafe { shim_ffi::abx_object_release(self.device_box) };
        }
    }
}
