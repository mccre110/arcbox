//! Pure USB accessory registry: connect/disconnect and attach bookkeeping.
//!
//! Generic over the platform handle types (`A` = accessory, `D` = attached
//! device) so the state machine is unit-testable without macOS handles. The
//! macOS [`UsbManager`](super::UsbManager) instantiates it with
//! `Arc<arcbox_vz::UsbAccessory>` / `Arc<arcbox_vz::UsbDevice>`.

// Only the macOS UsbManager constructs a registry; the state machine still
// compiles (and its tests run) on every platform.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::BTreeMap;

use crate::error::{CoreError, Result};

use super::{UsbDeviceInfo, UsbDeviceSnapshot, UsbSelector};

/// Attachment state of a connected accessory.
pub(crate) enum AttachState<D> {
    /// Granted to the daemon, not attached to a VM.
    Available,
    /// An attach operation is in flight (prevents double attach).
    Attaching,
    /// Attached to the System VM.
    Attached(D),
}

/// One connected accessory.
pub(crate) struct UsbEntry<A, D> {
    pub(crate) info: UsbDeviceInfo,
    pub(crate) accessory: A,
    pub(crate) state: AttachState<D>,
}

/// Registry of accessories currently granted to the daemon.
///
/// Keyed by IORegistry ID (`BTreeMap` for stable listing order). A replug
/// produces a new registry ID, so connect events never collide with live
/// entries in practice; a duplicate ID replaces the stale entry.
pub(crate) struct UsbRegistry<A, D> {
    entries: BTreeMap<u64, UsbEntry<A, D>>,
}

impl<A: Clone, D: Clone> UsbRegistry<A, D> {
    pub(crate) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Records a connected accessory (replacing any stale entry).
    pub(crate) fn connect(&mut self, info: UsbDeviceInfo, accessory: A) {
        let registry_id = info.registry_id;
        self.entries.insert(
            registry_id,
            UsbEntry {
                info,
                accessory,
                state: AttachState::Available,
            },
        );
    }

    /// Removes a disconnected accessory, returning its entry (so the caller
    /// can observe whether it was attached and drop the handles).
    pub(crate) fn disconnect(&mut self, registry_id: u64) -> Option<UsbEntry<A, D>> {
        self.entries.remove(&registry_id)
    }

    /// Lists all connected accessories.
    pub(crate) fn list(&self) -> Vec<UsbDeviceSnapshot> {
        self.entries
            .values()
            .map(|entry| UsbDeviceSnapshot {
                info: entry.info.clone(),
                attached: matches!(entry.state, AttachState::Attached(_)),
                guest_path: None,
            })
            .collect()
    }

    /// Resolves a selector to exactly one connected accessory.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when nothing matches and an invalid-state
    /// error when the selector is ambiguous.
    pub(crate) fn resolve(&self, selector: &UsbSelector) -> Result<u64> {
        let mut matches = self
            .entries
            .values()
            .filter(|entry| selector.matches(&entry.info));
        let first = matches
            .next()
            .ok_or_else(|| CoreError::not_found(format!("USB device {selector}")))?;
        if matches.next().is_some() {
            return Err(CoreError::invalid_state(format!(
                "USB selector {selector} matches multiple devices; disambiguate with a serial \
                 (vid:pid:serial)"
            )));
        }
        Ok(first.info.registry_id)
    }

    /// Starts an attach: resolves the selector, transitions the entry to
    /// `Attaching`, and hands out the accessory handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the selector does not resolve or the device is
    /// already attached (or being attached).
    pub(crate) fn begin_attach(&mut self, selector: &UsbSelector) -> Result<(u64, A)> {
        let registry_id = self.resolve(selector)?;
        let entry = self
            .entries
            .get_mut(&registry_id)
            .expect("resolved entry exists");
        match entry.state {
            AttachState::Available => {
                entry.state = AttachState::Attaching;
                Ok((registry_id, entry.accessory.clone()))
            }
            AttachState::Attaching => Err(CoreError::invalid_state(format!(
                "USB device {selector} attach already in progress"
            ))),
            AttachState::Attached(_) => Err(CoreError::invalid_state(format!(
                "USB device {selector} is already attached"
            ))),
        }
    }

    /// Completes an attach. When the entry vanished meanwhile (accessory
    /// disconnected mid-attach) the device handle is simply dropped.
    pub(crate) fn complete_attach(&mut self, registry_id: u64, device: D) {
        if let Some(entry) = self.entries.get_mut(&registry_id) {
            entry.state = AttachState::Attached(device);
        }
    }

    /// Reverts a failed attach to `Available`.
    pub(crate) fn abort_attach(&mut self, registry_id: u64) {
        if let Some(entry) = self.entries.get_mut(&registry_id)
            && matches!(entry.state, AttachState::Attaching)
        {
            entry.state = AttachState::Available;
        }
    }

    /// Starts a detach: resolves the selector and hands out the attached
    /// device handle. The entry stays `Attached` until
    /// [`complete_detach`](Self::complete_detach) so a failed detach keeps
    /// the bookkeeping truthful.
    ///
    /// # Errors
    ///
    /// Returns an error when the selector does not resolve or the device is
    /// not attached.
    pub(crate) fn begin_detach(&mut self, selector: &UsbSelector) -> Result<(u64, D)> {
        let registry_id = self.resolve(selector)?;
        let entry = self
            .entries
            .get(&registry_id)
            .expect("resolved entry exists");
        match &entry.state {
            AttachState::Attached(device) => Ok((registry_id, device.clone())),
            _ => Err(CoreError::invalid_state(format!(
                "USB device {selector} is not attached"
            ))),
        }
    }

    /// Completes a detach, dropping the device handle.
    pub(crate) fn complete_detach(&mut self, registry_id: u64) {
        if let Some(entry) = self.entries.get_mut(&registry_id) {
            entry.state = AttachState::Available;
        }
    }

    /// Drops every attachment, reverting all entries to `Available`.
    ///
    /// Called when the System VM stops: attachments are runtime-only, and
    /// the device handles refer to a VM that no longer exists.
    pub(crate) fn reset_attachments(&mut self) {
        for entry in self.entries.values_mut() {
            entry.state = AttachState::Available;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(registry_id: u64, vid: u16, pid: u16, serial: Option<&str>) -> UsbDeviceInfo {
        UsbDeviceInfo {
            registry_id,
            vendor_id: vid,
            product_id: pid,
            name: Some(format!("dev-{registry_id}")),
            serial: serial.map(str::to_string),
        }
    }

    fn selector(vid: u16, pid: u16, serial: Option<&str>) -> UsbSelector {
        UsbSelector {
            vendor_id: vid,
            product_id: pid,
            serial: serial.map(str::to_string),
        }
    }

    #[test]
    fn connect_disconnect_lifecycle() {
        let mut reg: UsbRegistry<(), ()> = UsbRegistry::new();
        reg.connect(info(1, 0x1234, 0x5678, None), ());
        assert_eq!(reg.list().len(), 1);
        assert!(!reg.list()[0].attached);

        assert!(reg.disconnect(1).is_some());
        assert!(reg.disconnect(1).is_none());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn resolve_by_vid_pid_and_serial() {
        let mut reg: UsbRegistry<(), ()> = UsbRegistry::new();
        reg.connect(info(1, 0x1234, 0x5678, Some("AAA")), ());
        reg.connect(info(2, 0x1234, 0x5678, Some("BBB")), ());
        reg.connect(info(3, 0xffff, 0x0001, None), ());

        // Unique vid:pid resolves without a serial.
        assert_eq!(reg.resolve(&selector(0xffff, 0x0001, None)).unwrap(), 3);
        // Duplicate vid:pid needs the serial.
        assert!(reg.resolve(&selector(0x1234, 0x5678, None)).is_err());
        assert_eq!(
            reg.resolve(&selector(0x1234, 0x5678, Some("BBB"))).unwrap(),
            2
        );
        // No match.
        assert!(reg.resolve(&selector(0x0000, 0x0000, None)).is_err());
    }

    #[test]
    fn attach_state_machine() {
        let mut reg: UsbRegistry<(), u32> = UsbRegistry::new();
        reg.connect(info(1, 0x1234, 0x5678, None), ());
        let sel = selector(0x1234, 0x5678, None);

        // Detach before attach is rejected.
        assert!(reg.begin_detach(&sel).is_err());

        let (id, ()) = reg.begin_attach(&sel).unwrap();
        assert_eq!(id, 1);
        // Double attach (in progress) is rejected.
        assert!(reg.begin_attach(&sel).is_err());

        reg.complete_attach(id, 42);
        assert!(reg.list()[0].attached);
        // Attach on an attached device is rejected.
        assert!(reg.begin_attach(&sel).is_err());

        let (id, device) = reg.begin_detach(&sel).unwrap();
        assert_eq!(device, 42);
        // Still attached until the detach completes.
        assert!(reg.list()[0].attached);
        reg.complete_detach(id);
        assert!(!reg.list()[0].attached);

        // Full cycle again works.
        assert!(reg.begin_attach(&sel).is_ok());
    }

    #[test]
    fn failed_attach_reverts_to_available() {
        let mut reg: UsbRegistry<(), u32> = UsbRegistry::new();
        reg.connect(info(1, 0x1234, 0x5678, None), ());
        let sel = selector(0x1234, 0x5678, None);

        let (id, ()) = reg.begin_attach(&sel).unwrap();
        reg.abort_attach(id);
        assert!(!reg.list()[0].attached);
        assert!(reg.begin_attach(&sel).is_ok());
    }

    #[test]
    fn vm_stop_resets_attachments() {
        let mut reg: UsbRegistry<(), u32> = UsbRegistry::new();
        reg.connect(info(1, 0x1234, 0x5678, None), ());
        let sel = selector(0x1234, 0x5678, None);

        let (id, ()) = reg.begin_attach(&sel).unwrap();
        reg.complete_attach(id, 42);
        assert!(reg.list()[0].attached);

        reg.reset_attachments();
        assert!(!reg.list()[0].attached);
        assert!(reg.begin_attach(&sel).is_ok());
    }

    #[test]
    fn disconnect_while_attaching_drops_late_completion() {
        let mut reg: UsbRegistry<(), u32> = UsbRegistry::new();
        reg.connect(info(1, 0x1234, 0x5678, None), ());
        let sel = selector(0x1234, 0x5678, None);

        let (id, ()) = reg.begin_attach(&sel).unwrap();
        assert!(reg.disconnect(id).is_some());
        // The late completion for a vanished entry is a no-op.
        reg.complete_attach(id, 42);
        assert!(reg.list().is_empty());
    }
}
