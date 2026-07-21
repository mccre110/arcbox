//! Guest USB device listing from sysfs.
//!
//! Enumerates `/sys/bus/usb/devices` so the host can map each passed-through
//! accessory to its guest device node (`/dev/bus/usb/BBB/DDD`) — the path a
//! user hands to `docker run --device`.

use std::path::Path;

use arcbox_protocol::agent::GuestUsbDevice;

/// Default sysfs USB device directory.
const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Lists USB devices visible in the guest.
///
/// Missing sysfs (USB core not compiled in, nothing attached yet) yields an
/// empty list rather than an error: an empty guest bus is a normal state.
pub(super) fn list_guest_usb_devices() -> Vec<GuestUsbDevice> {
    list_usb_devices_in(Path::new(SYSFS_USB_DEVICES))
}

/// Enumerates USB devices under a sysfs `devices` directory.
///
/// Device entries (`1-1`, `usb1`) carry `busnum`/`devnum`/`idVendor`/
/// `idProduct` attribute files; interface entries (`1-1:1.0`) do not and are
/// skipped by the same missing-attribute check that skips unreadable entries.
fn list_usb_devices_in(dir: &Path) -> Vec<GuestUsbDevice> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut devices: Vec<GuestUsbDevice> = entries
        .flatten()
        .filter_map(|entry| parse_usb_device(&entry.path()))
        .collect();
    // read_dir order is arbitrary; sort for a stable listing.
    devices.sort_by(|a, b| a.dev_path.cmp(&b.dev_path));
    devices
}

/// Parses one sysfs USB device directory, returning `None` when any
/// required attribute is missing or malformed (interface entries, hubs with
/// unreadable attributes, or corrupt sysfs content).
fn parse_usb_device(path: &Path) -> Option<GuestUsbDevice> {
    let busnum: u32 = read_attr(path, "busnum")?.parse().ok()?;
    let devnum: u32 = read_attr(path, "devnum")?.parse().ok()?;
    let vendor_id = u16::from_str_radix(&read_attr(path, "idVendor")?, 16).ok()?;
    let product_id = u16::from_str_radix(&read_attr(path, "idProduct")?, 16).ok()?;
    // Serial is optional — most hubs and many devices have none.
    let serial = read_attr(path, "serial").unwrap_or_default();

    Some(GuestUsbDevice {
        vendor_id: u32::from(vendor_id),
        product_id: u32::from(product_id),
        serial,
        dev_path: format!("/dev/bus/usb/{busnum:03}/{devnum:03}"),
    })
}

/// Reads and trims one sysfs attribute file.
fn read_attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Writes a fake sysfs USB device directory.
    fn write_device(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        for (attr, value) in attrs {
            fs::write(dir.join(attr), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn parses_devices_and_skips_interfaces() {
        let tmp = tempfile::tempdir().unwrap();
        write_device(
            tmp.path(),
            "1-1",
            &[
                ("busnum", "1"),
                ("devnum", "2"),
                ("idVendor", "0403"),
                ("idProduct", "6001"),
                ("serial", "FTA1B2C3"),
            ],
        );
        // Interface entry: no busnum/devnum/id attributes.
        write_device(tmp.path(), "1-1:1.0", &[("bInterfaceClass", "ff")]);
        // Root hub, no serial.
        write_device(
            tmp.path(),
            "usb1",
            &[
                ("busnum", "1"),
                ("devnum", "1"),
                ("idVendor", "1d6b"),
                ("idProduct", "0002"),
            ],
        );

        let devices = list_usb_devices_in(tmp.path());
        assert_eq!(devices.len(), 2);

        assert_eq!(devices[0].dev_path, "/dev/bus/usb/001/001");
        assert_eq!(devices[0].vendor_id, 0x1d6b);
        assert_eq!(devices[0].product_id, 0x0002);
        assert_eq!(devices[0].serial, "");

        assert_eq!(devices[1].dev_path, "/dev/bus/usb/001/002");
        assert_eq!(devices[1].vendor_id, 0x0403);
        assert_eq!(devices[1].product_id, 0x6001);
        assert_eq!(devices[1].serial, "FTA1B2C3");
    }

    #[test]
    fn malformed_attributes_skip_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        write_device(
            tmp.path(),
            "2-1",
            &[
                ("busnum", "not-a-number"),
                ("devnum", "2"),
                ("idVendor", "0403"),
                ("idProduct", "6001"),
            ],
        );
        // Vendor ID out of u16 hex range.
        write_device(
            tmp.path(),
            "2-2",
            &[
                ("busnum", "2"),
                ("devnum", "3"),
                ("idVendor", "fffff"),
                ("idProduct", "6001"),
            ],
        );
        assert!(list_usb_devices_in(tmp.path()).is_empty());
    }

    #[test]
    fn missing_directory_is_empty() {
        assert!(list_usb_devices_in(Path::new("/nonexistent/usb")).is_empty());
    }
}
