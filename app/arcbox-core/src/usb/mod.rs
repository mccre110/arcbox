//! USB passthrough orchestration (macOS 27+ Accessory Access, VZ backend).
//!
//! The daemon registers a process-lifetime accessory listener at startup
//! ([`UsbManager::start`]); accessories the user grants through the macOS
//! Accessory Access UI land in a registry keyed by IORegistry ID. `arcbox
//! usb attach|detach` hot-plugs a granted accessory into the running System
//! VM by `vid:pid[:serial]` selector ([`UsbSelector`]).
//!
//! Attachments are runtime-only: a System VM stop resets every entry to
//! available. On other platforms (and on macOS without Accessory Access)
//! listing returns empty and attach/detach fail with a clear error.

mod manager;
mod registry;
mod selector;

pub use manager::UsbManager;
pub use selector::UsbSelector;

/// Identity of a host USB accessory granted to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceInfo {
    /// IORegistry entry ID — stable for the duration of a connection.
    pub registry_id: u64,
    /// USB vendor ID (`idVendor`).
    pub vendor_id: u16,
    /// USB product ID (`idProduct`).
    pub product_id: u16,
    /// Product name, when the host reports one.
    pub name: Option<String>,
    /// Serial number, when the host reports one.
    pub serial: Option<String>,
}

#[cfg(target_os = "macos")]
impl From<arcbox_vz::UsbAccessoryInfo> for UsbDeviceInfo {
    fn from(info: arcbox_vz::UsbAccessoryInfo) -> Self {
        Self {
            registry_id: info.registry_id,
            vendor_id: info.vendor_id,
            product_id: info.product_id,
            name: info.name,
            serial: info.serial,
        }
    }
}

/// Point-in-time view of one granted accessory, as returned by
/// [`UsbManager::list`].
#[derive(Debug, Clone)]
pub struct UsbDeviceSnapshot {
    /// The accessory's identity.
    pub info: UsbDeviceInfo,
    /// Whether the accessory is attached to the System VM.
    pub attached: bool,
    /// Guest device node (`/dev/bus/usb/BBB/DDD`) when attached and matched
    /// against the guest USB listing (see [`annotate_guest_paths`]).
    pub guest_path: Option<String>,
}

/// One USB device as enumerated inside the guest (from sysfs, via the
/// agent's guest USB listing RPC).
#[derive(Debug, Clone)]
pub struct GuestUsbDevice {
    /// USB vendor ID (`idVendor`).
    pub vendor_id: u16,
    /// USB product ID (`idProduct`).
    pub product_id: u16,
    /// Serial number, when the device reports one.
    pub serial: Option<String>,
    /// Guest device node, e.g. `/dev/bus/usb/001/002`.
    pub dev_path: String,
}

/// Fills [`UsbDeviceSnapshot::guest_path`] on attached snapshots by matching
/// them against the guest USB listing.
///
/// Serial-exact matches win; remaining attached snapshots fall back to the
/// first unclaimed `vid:pid` match. Each guest device is claimed by at most
/// one snapshot, so duplicate serial-less devices pair off greedily instead
/// of all reporting the same node.
pub fn annotate_guest_paths(snapshots: &mut [UsbDeviceSnapshot], guest_devices: &[GuestUsbDevice]) {
    let mut claimed = vec![false; guest_devices.len()];

    for snapshot in snapshots.iter_mut().filter(|s| s.attached) {
        let Some(serial) = snapshot.info.serial.as_deref() else {
            continue;
        };
        let matched = guest_devices.iter().enumerate().find(|(i, dev)| {
            !claimed[*i]
                && dev.vendor_id == snapshot.info.vendor_id
                && dev.product_id == snapshot.info.product_id
                && dev.serial.as_deref() == Some(serial)
        });
        if let Some((i, dev)) = matched {
            claimed[i] = true;
            snapshot.guest_path = Some(dev.dev_path.clone());
        }
    }

    for snapshot in snapshots
        .iter_mut()
        .filter(|s| s.attached && s.guest_path.is_none())
    {
        let matched = guest_devices.iter().enumerate().find(|(i, dev)| {
            !claimed[*i]
                && dev.vendor_id == snapshot.info.vendor_id
                && dev.product_id == snapshot.info.product_id
        });
        if let Some((i, dev)) = matched {
            claimed[i] = true;
            snapshot.guest_path = Some(dev.dev_path.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(vid: u16, pid: u16, serial: Option<&str>, attached: bool) -> UsbDeviceSnapshot {
        UsbDeviceSnapshot {
            info: UsbDeviceInfo {
                registry_id: 0,
                vendor_id: vid,
                product_id: pid,
                name: None,
                serial: serial.map(str::to_string),
            },
            attached,
            guest_path: None,
        }
    }

    fn guest(vid: u16, pid: u16, serial: Option<&str>, path: &str) -> GuestUsbDevice {
        GuestUsbDevice {
            vendor_id: vid,
            product_id: pid,
            serial: serial.map(str::to_string),
            dev_path: path.to_string(),
        }
    }

    #[test]
    fn serial_match_wins_over_vid_pid_order() {
        let mut snapshots = vec![snapshot(0x1234, 0x5678, Some("BBB"), true)];
        let devices = vec![
            guest(0x1234, 0x5678, Some("AAA"), "/dev/bus/usb/001/002"),
            guest(0x1234, 0x5678, Some("BBB"), "/dev/bus/usb/001/003"),
        ];
        annotate_guest_paths(&mut snapshots, &devices);
        assert_eq!(
            snapshots[0].guest_path.as_deref(),
            Some("/dev/bus/usb/001/003")
        );
    }

    #[test]
    fn serial_less_duplicates_pair_off() {
        let mut snapshots = vec![
            snapshot(0x1234, 0x5678, None, true),
            snapshot(0x1234, 0x5678, None, true),
        ];
        let devices = vec![
            guest(0x1234, 0x5678, None, "/dev/bus/usb/001/002"),
            guest(0x1234, 0x5678, None, "/dev/bus/usb/001/003"),
        ];
        annotate_guest_paths(&mut snapshots, &devices);
        assert_eq!(
            snapshots[0].guest_path.as_deref(),
            Some("/dev/bus/usb/001/002")
        );
        assert_eq!(
            snapshots[1].guest_path.as_deref(),
            Some("/dev/bus/usb/001/003")
        );
    }

    #[test]
    fn detached_and_unmatched_stay_unannotated() {
        let mut snapshots = vec![
            snapshot(0x1234, 0x5678, None, false),
            snapshot(0xffff, 0x0001, None, true),
        ];
        let devices = vec![guest(0x1234, 0x5678, None, "/dev/bus/usb/001/002")];
        annotate_guest_paths(&mut snapshots, &devices);
        assert!(snapshots[0].guest_path.is_none());
        assert!(snapshots[1].guest_path.is_none());
    }
}
