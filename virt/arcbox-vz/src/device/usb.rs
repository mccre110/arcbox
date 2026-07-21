//! USB (XHCI) controller configuration.
//!
//! A VM needs a USB controller in its configuration before USB devices can
//! be hot-plugged at runtime (see [`crate::usb`]). Requires macOS 27+ — use
//! [`crate::usb_passthrough_supported`] before constructing one.

use crate::error::{VZError, VZResult};
use std::ffi::c_void;
use std::ptr;

/// Configuration for a USB XHCI controller.
pub struct UsbControllerConfiguration {
    inner: *mut c_void,
}

// SAFETY: The inner pointer is an ObjC object handle created by the shim;
// all access goes through the shim.
unsafe impl Send for UsbControllerConfiguration {}

impl UsbControllerConfiguration {
    /// Creates a new XHCI controller configuration.
    ///
    /// # Errors
    ///
    /// Returns [`VZError::NotSupported`] when the host (or the SDK the
    /// binary was built with) does not support USB passthrough.
    pub fn new() -> VZResult<Self> {
        // SAFETY: on success the shim returns a +1 handle, released by Drop;
        // on failure it writes a strdup'd message that take_error_string
        // frees.
        unsafe {
            let mut error: *mut std::ffi::c_char = ptr::null_mut();
            let obj = crate::shim_ffi::abx_usb_xhci_config_new(&raw mut error);
            if obj.is_null() {
                let _ = crate::shim_ffi::take_error_string(error);
                return Err(VZError::NotSupported);
            }
            Ok(Self { inner: obj })
        }
    }

    /// Consumes the configuration and returns the raw pointer.
    #[must_use]
    pub(crate) fn into_ptr(self) -> *mut c_void {
        let ptr = self.inner;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for UsbControllerConfiguration {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            // SAFETY: releasing the +1 handle returned by the shim.
            unsafe { crate::shim_ffi::abx_object_release(self.inner.cast()) };
        }
    }
}
