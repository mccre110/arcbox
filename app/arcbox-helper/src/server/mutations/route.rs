//! Route management via PF_ROUTE routing socket.
//!
//! `arcbox-route` is macOS-only (PF_ROUTE); on other platforms the
//! mutations fail with a clear error so the helper binary still compiles.

use arcbox_helper::HelperError;
use arcbox_helper::validate::{BridgeIface, Subnet};
#[cfg(target_os = "macos")]
use arcbox_route::Ipv4Net;

/// Adds a route for `subnet` via `iface`.
#[cfg(target_os = "macos")]
pub fn add(subnet: &Subnet, iface: &BridgeIface) -> Result<(), HelperError> {
    let net = to_ipv4net(subnet)?;
    arcbox_route::add(net, iface.as_str()).map_err(HelperError::other)
}

/// Removes the route for `subnet`.
#[cfg(target_os = "macos")]
pub fn remove(subnet: &Subnet) -> Result<(), HelperError> {
    let net = to_ipv4net(subnet)?;
    arcbox_route::remove(net).map_err(HelperError::other)
}

#[cfg(target_os = "macos")]
fn to_ipv4net(subnet: &Subnet) -> Result<Ipv4Net, HelperError> {
    let inner = subnet.network();
    Ipv4Net::new(inner.ip(), inner.prefix())
        .map_err(|e| HelperError::other(format!("invalid subnet: {e}")))
}

/// Non-macOS stub: PF_ROUTE route management requires macOS.
#[cfg(not(target_os = "macos"))]
pub fn add(_subnet: &Subnet, _iface: &BridgeIface) -> Result<(), HelperError> {
    Err(HelperError::other("route management requires macOS"))
}

/// Non-macOS stub: PF_ROUTE route management requires macOS.
#[cfg(not(target_os = "macos"))]
pub fn remove(_subnet: &Subnet) -> Result<(), HelperError> {
    Err(HelperError::other("route management requires macOS"))
}
