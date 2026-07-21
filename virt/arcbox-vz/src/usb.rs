//! USB passthrough via the macOS 27 Accessory Access framework.
//!
//! The user grants USB accessories to the application through the system
//! Accessory Access UI. A process-lifetime listener (registered once via
//! [`register_accessory_listener`]) delivers connect/disconnect events;
//! granted accessories are hot-plugged into a running VM through
//! [`UsbController::attach_blocking`] and removed with
//! [`UsbController::detach_blocking`].
//!
//! Requires macOS 27+ and a binary built against the macOS 27 SDK; every
//! entry point returns [`VZError`]s otherwise (probe with
//! [`crate::usb_passthrough_supported`]).

use crate::error::{VZError, VZResult};
use crate::shim_ffi;
use crate::vm::state_trampoline;
use std::ffi::{c_char, c_void};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

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

/// Exactly-once attach trampoline: consumes the boxed sender.
///
/// The result carries the [`UsbDevice`] so every abandonment path (dropped
/// receiver, or a buffered value in a channel no one reads) releases the +1
/// device handle via `UsbDevice`'s Drop.
unsafe extern "C" fn usb_attach_trampoline(
    ctx: *mut c_void,
    handle: *mut c_void,
    err: *mut c_char,
) {
    // SAFETY: ctx is the Box<Sender> leaked in attach_blocking; the shim
    // guarantees exactly-once invocation. err is null or a shim string that
    // take_error_string frees.
    unsafe {
        let sender = Box::from_raw(ctx.cast::<std_mpsc::Sender<Result<UsbDevice, String>>>());
        let result = if err.is_null() {
            Ok(UsbDevice { device_box: handle })
        } else {
            Err(shim_ffi::take_error_string(err))
        };
        let _ = sender.send(result);
    }
}

/// Exactly-once detach trampoline: consumes the boxed sender.
unsafe extern "C" fn usb_detach_trampoline(ctx: *mut c_void, err: *mut c_char) {
    // SAFETY: ctx is the Box<Sender> leaked in detach_blocking; the shim
    // guarantees exactly-once invocation. err is null or a shim string that
    // take_error_string frees.
    unsafe {
        let sender = Box::from_raw(ctx.cast::<std_mpsc::Sender<Result<(), String>>>());
        let result = if err.is_null() {
            Ok(())
        } else {
            Err(shim_ffi::take_error_string(err))
        };
        let _ = sender.send(result);
    }
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

    /// Attaches a granted accessory to this controller (hot-plug), blocking
    /// until the framework confirms or `timeout` elapses.
    ///
    /// Returns the attached device, needed later for
    /// [`detach_blocking`](Self::detach_blocking).
    ///
    /// # Errors
    ///
    /// Returns an error when the framework rejects the attach (unsupported
    /// device type, accessory revoked, VM not running, ...) or on timeout.
    pub fn attach_blocking(
        &self,
        accessory: &UsbAccessory,
        timeout: Duration,
    ) -> VZResult<UsbDevice> {
        let (tx, rx) = std_mpsc::channel::<Result<UsbDevice, String>>();
        let ctx: *mut c_void = Box::into_raw(Box::new(tx)).cast();
        // SAFETY: both handles are valid; ctx ownership transfers to the
        // exactly-once trampoline. A timed-out receiver is leak-free: the
        // trampoline's send fails (or the buffered UsbDevice drops with the
        // channel) and the +1 device handle is released either way.
        unsafe {
            shim_ffi::abx_usb_controller_attach(
                self.controller_box,
                accessory.handle,
                ctx,
                usb_attach_trampoline,
            );
        }

        match rx.recv_timeout(timeout) {
            Ok(Ok(device)) => Ok(device),
            Ok(Err(msg)) => Err(VZError::OperationFailed(format!(
                "USB attach failed: {msg}"
            ))),
            Err(_) => Err(VZError::Timeout(format!(
                "USB attach timed out after {timeout:?}"
            ))),
        }
    }

    /// Detaches a previously attached device (hot-unplug), blocking until
    /// the framework confirms or `timeout` elapses.
    ///
    /// # Errors
    ///
    /// Returns an error when the framework rejects the detach or on timeout.
    pub fn detach_blocking(&self, device: &UsbDevice, timeout: Duration) -> VZResult<()> {
        let (tx, rx) = std_mpsc::channel::<Result<(), String>>();
        let ctx: *mut c_void = Box::into_raw(Box::new(tx)).cast();
        // SAFETY: both handles are valid; ctx ownership transfers to the
        // exactly-once trampoline.
        unsafe {
            shim_ffi::abx_usb_controller_detach(
                self.controller_box,
                device.device_box,
                ctx,
                usb_detach_trampoline,
            );
        }

        match rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => Err(VZError::OperationFailed(format!(
                "USB detach failed: {msg}"
            ))),
            Err(_) => Err(VZError::Timeout(format!(
                "USB detach timed out after {timeout:?}"
            ))),
        }
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
