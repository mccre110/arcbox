//! USB passthrough commands (macOS 27+ Accessory Access, VZ backend).
//!
//! Devices are granted to ArcBox through the macOS Accessory Access UI;
//! `arcbox usb list` shows the granted accessories and `attach`/`detach`
//! hot-plug them into the running System VM by `vid:pid[:serial]` selector.

use anyhow::{Context, Result};
use arcbox_core::UsbSelector;
use arcbox_protocol::v1::{
    AttachUsbDeviceRequest, DetachUsbDeviceRequest, ListUsbDevicesRequest, UsbDevice,
    UsbDeviceSelector,
};
use clap::Subcommand;
use tonic::Request;

use super::system::system_client;

#[derive(Debug, Subcommand)]
pub enum UsbCommands {
    /// List USB devices granted to ArcBox and their attachment state.
    ///
    /// Grant devices through the macOS Accessory Access UI first; devices
    /// never granted to ArcBox do not appear here.
    List,

    /// Attach a granted USB device to the running System VM (hot-plug).
    ///
    /// Once attached, the device appears in the guest under /dev/bus/usb
    /// and can be mapped into containers with `docker run --device`.
    Attach {
        /// Device selector: vid:pid[:serial], hex vid/pid (e.g. 0403:6001).
        selector: String,
    },

    /// Detach an attached USB device from the System VM.
    Detach {
        /// Device selector: vid:pid[:serial], hex vid/pid (e.g. 0403:6001).
        selector: String,
    },
}

/// Parses the CLI selector argument and converts it to the wire message.
fn parse_selector(input: &str) -> Result<UsbDeviceSelector> {
    let selector: UsbSelector = input.parse()?;
    Ok(UsbDeviceSelector {
        vendor_id: u32::from(selector.vendor_id),
        product_id: u32::from(selector.product_id),
        serial: selector.serial.unwrap_or_default(),
    })
}

/// Formats one device row for the list table.
fn device_row(device: &UsbDevice) -> String {
    let id = format!("{:04x}:{:04x}", device.vendor_id, device.product_id);
    let name = if device.name.is_empty() {
        "-"
    } else {
        &device.name
    };
    let serial = if device.serial.is_empty() {
        "-"
    } else {
        &device.serial
    };
    let state = if device.attached {
        "attached"
    } else {
        "available"
    };
    let guest_path = if device.guest_path.is_empty() {
        "-"
    } else {
        &device.guest_path
    };
    format!("{id:<10} {name:<28} {serial:<20} {state:<10} {guest_path}")
}

pub async fn execute(cmd: UsbCommands) -> Result<()> {
    let mut client = system_client().await?;
    match cmd {
        UsbCommands::List => {
            let devices = client
                .list_usb_devices(Request::new(ListUsbDevicesRequest {}))
                .await
                .context("failed to list USB devices")?
                .into_inner()
                .devices;

            if devices.is_empty() {
                println!(
                    "No USB devices granted to ArcBox.\n\
                     Grant devices via the macOS Accessory Access UI (requires macOS 27+)."
                );
                return Ok(());
            }

            println!(
                "{:<10} {:<28} {:<20} {:<10} GUEST PATH",
                "VID:PID", "NAME", "SERIAL", "STATE"
            );
            for device in &devices {
                println!("{}", device_row(device));
            }
        }
        UsbCommands::Attach { selector } => {
            let selector = parse_selector(&selector)?;
            client
                .attach_usb_device(Request::new(AttachUsbDeviceRequest {
                    selector: Some(selector.clone()),
                }))
                .await
                .context("failed to attach USB device")?;
            println!(
                "Attached {:04x}:{:04x} to the System VM.",
                selector.vendor_id, selector.product_id
            );
        }
        UsbCommands::Detach { selector } => {
            let selector = parse_selector(&selector)?;
            client
                .detach_usb_device(Request::new(DetachUsbDeviceRequest {
                    selector: Some(selector.clone()),
                }))
                .await
                .context("failed to detach USB device")?;
            println!(
                "Detached {:04x}:{:04x} from the System VM.",
                selector.vendor_id, selector.product_id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_maps_to_wire_message() {
        let wire = parse_selector("0403:6001:FTA1B2C3").unwrap();
        assert_eq!(wire.vendor_id, 0x0403);
        assert_eq!(wire.product_id, 0x6001);
        assert_eq!(wire.serial, "FTA1B2C3");

        let wire = parse_selector("04fb:c532").unwrap();
        assert_eq!(wire.vendor_id, 0x04fb);
        assert_eq!(wire.product_id, 0xc532);
        assert_eq!(wire.serial, "");
    }

    #[test]
    fn selector_rejects_malformed_input() {
        for input in ["", "0403", "0403:60011", "xyz:6001"] {
            assert!(parse_selector(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn list_rows_render_placeholders_and_values() {
        let attached = UsbDevice {
            vendor_id: 0x0403,
            product_id: 0x6001,
            name: "FT232R".to_string(),
            serial: "A1B2".to_string(),
            attached: true,
            guest_path: "/dev/bus/usb/001/002".to_string(),
        };
        assert_eq!(
            device_row(&attached),
            "0403:6001  FT232R                       A1B2                 attached   /dev/bus/usb/001/002"
        );

        let bare = UsbDevice {
            vendor_id: 0xffff,
            product_id: 0x0001,
            name: String::new(),
            serial: String::new(),
            attached: false,
            guest_path: String::new(),
        };
        assert_eq!(
            device_row(&bare),
            "ffff:0001  -                            -                    available  -"
        );
    }
}
