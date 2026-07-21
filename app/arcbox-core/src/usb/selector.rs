//! `vid:pid[:serial]` USB device selector.

use std::fmt;
use std::str::FromStr;

use crate::error::CoreError;

use super::UsbDeviceInfo;

/// Selects a USB device by vendor/product ID with an optional serial to
/// disambiguate identical devices. Parsed from `vid:pid[:serial]` where
/// `vid`/`pid` are 4-digit-max hex (e.g. `0403:6001` or `0403:6001:FTA1B2C3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbSelector {
    /// USB vendor ID (`idVendor`).
    pub vendor_id: u16,
    /// USB product ID (`idProduct`).
    pub product_id: u16,
    /// Serial number, required only when several `vid:pid` devices are
    /// connected.
    pub serial: Option<String>,
}

impl UsbSelector {
    /// Whether this selector matches the given device identity.
    #[must_use]
    pub fn matches(&self, info: &UsbDeviceInfo) -> bool {
        self.vendor_id == info.vendor_id
            && self.product_id == info.product_id
            && self
                .serial
                .as_deref()
                .is_none_or(|serial| info.serial.as_deref() == Some(serial))
    }
}

impl fmt::Display for UsbSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor_id, self.product_id)?;
        if let Some(serial) = &self.serial {
            write!(f, ":{serial}")?;
        }
        Ok(())
    }
}

impl FromStr for UsbSelector {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            CoreError::config(format!(
                "invalid USB selector '{s}': expected vid:pid[:serial] with 4-digit-max hex \
                 vid/pid (e.g. 0403:6001)"
            ))
        };

        // The serial may itself contain ':' — split off vid and pid only.
        let mut parts = s.splitn(3, ':');
        let vid = parts.next().ok_or_else(invalid)?;
        let pid = parts.next().ok_or_else(invalid)?;
        let serial = parts.next();

        let parse_hex = |part: &str| {
            if part.is_empty() || part.len() > 4 {
                return Err(invalid());
            }
            u16::from_str_radix(part, 16).map_err(|_| invalid())
        };

        Ok(Self {
            vendor_id: parse_hex(vid)?,
            product_id: parse_hex(pid)?,
            serial: match serial {
                Some("") | None => None,
                Some(serial) => Some(serial.to_string()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vid_pid() {
        let sel: UsbSelector = "0403:6001".parse().unwrap();
        assert_eq!(sel.vendor_id, 0x0403);
        assert_eq!(sel.product_id, 0x6001);
        assert_eq!(sel.serial, None);
        assert_eq!(sel.to_string(), "0403:6001");
    }

    #[test]
    fn parses_serial_including_colons() {
        let sel: UsbSelector = "0403:6001:FT:A1".parse().unwrap();
        assert_eq!(sel.serial.as_deref(), Some("FT:A1"));
        assert_eq!(sel.to_string(), "0403:6001:FT:A1");
    }

    #[test]
    fn accepts_short_hex_and_uppercase() {
        let sel: UsbSelector = "403:1".parse().unwrap();
        assert_eq!(sel.vendor_id, 0x0403);
        assert_eq!(sel.product_id, 0x0001);
        let sel: UsbSelector = "04FB:C532".parse().unwrap();
        assert_eq!(sel.vendor_id, 0x04fb);
        assert_eq!(sel.product_id, 0xc532);
    }

    #[test]
    fn rejects_malformed() {
        for input in [
            "",
            "0403",
            "0403:",
            ":6001",
            "0403:60011",
            "xyz:6001",
            "0403:60g1",
        ] {
            assert!(input.parse::<UsbSelector>().is_err(), "accepted {input:?}");
        }
        // Trailing empty serial is treated as absent, not an error.
        let sel: UsbSelector = "0403:6001:".parse().unwrap();
        assert_eq!(sel.serial, None);
    }

    #[test]
    fn matching_honors_optional_serial() {
        let info = UsbDeviceInfo {
            registry_id: 1,
            vendor_id: 0x0403,
            product_id: 0x6001,
            name: None,
            serial: Some("AAA".to_string()),
        };
        let base: UsbSelector = "0403:6001".parse().unwrap();
        assert!(base.matches(&info));
        let exact: UsbSelector = "0403:6001:AAA".parse().unwrap();
        assert!(exact.matches(&info));
        let wrong: UsbSelector = "0403:6001:BBB".parse().unwrap();
        assert!(!wrong.matches(&info));
        let other: UsbSelector = "0403:6002".parse().unwrap();
        assert!(!other.matches(&info));
    }
}
