# USB Passthrough

ArcBox passes host USB devices through to the System VM using Apple's
Accessory Access framework (macOS 27+) and a Virtualization.framework
xHCI controller with `VZUSBPassthroughDevice` hot-plugging. Once a
device is attached, it appears in the guest under `/dev/bus/usb` and
can be mapped into containers with `docker run --device`.

## Requirements

- **macOS 27 or later.** The Accessory Access framework
  (`AAUSBAccessoryManager`) and `VZUSBPassthroughDevice` are new in
  macOS 27. On older hosts, `arcbox usb list` returns an empty list
  and attach/detach fail with a clear precondition error.
- **VZ backend.** The custom HV backend has no USB controller; attach
  requests on HV fail with `FAILED_PRECONDITION`. Switch with
  `arcbox system backend vz`.
- **Entitlement + provisioning.** The daemon binary must be signed
  with the `com.apple.developer.accessory-access.usb` entitlement
  ("Claim USB Accessory" capability), which is *restricted*: the
  Apple Developer provisioning profile must carry the capability, or
  registration is rejected at runtime (the daemon logs a warning and
  degrades to an empty listing). See `bundle/arcbox.entitlements`.
- **Guest kernel USB support.** The `arcboxlabs/kernel` build must
  have the USB core and xHCI driver compiled in. If the guest kernel
  lacks them, the device attaches on the host side but never appears
  in the guest.

## Flow

1. **Grant** — the user attaches a device to ArcBox through the macOS
   Accessory Access UI (menu extra). The daemon runs a
   process-lifetime accessory listener registered at startup; granted
   accessories land in its registry and show up in `arcbox usb list`
   as `available`.
2. **Attach** — `arcbox usb attach <vid:pid[:serial]>` hot-plugs the
   accessory into the running System VM's xHCI controller. The
   selector is `idVendor`/`idProduct` in hex, with an optional serial
   to disambiguate identical devices (e.g. `0403:6001` or
   `0403:6001:FTA1B2C3`).
3. **Use** — the guest enumerates the device under
   `/dev/bus/usb/BBB/DDD`. `arcbox usb list` shows the guest path for
   attached devices (matched against the guest's sysfs listing by
   vendor/product/serial). Map it into a container:

   ```bash
   arcbox usb attach 0403:6001
   arcbox usb list          # shows e.g. /dev/bus/usb/001/002
   docker run --device /dev/bus/usb/001/002 <image>
   ```

4. **Detach** — `arcbox usb detach <vid:pid[:serial]>` removes the
   device from the VM. Physically unplugging the device (or revoking
   the grant in the Accessory Access UI) detaches it implicitly.

## Docker device mapping

No ArcBox-specific handling is involved: `HostConfig.Devices`
(`--device`) passes through the Docker API proxy untouched to guest
dockerd, which maps the guest device node into the container via runc.
Anything visible in the guest's `/dev` works, including passed-through
USB device nodes.

## Semantics and limitations

- **Attachments are runtime-only.** A System VM stop or restart (or a
  backend switch) detaches everything; the accessories remain granted
  and can be re-attached. Persistent auto-attach is not implemented.
- **Grants belong to the daemon process.** Devices are granted to the
  signed daemon binary via the Accessory Access UI; the CLI only
  selects among already-granted accessories.
- **Selector ambiguity is an error.** When several granted devices
  share a `vid:pid`, attach/detach require the serial form; ArcBox
  refuses to pick one arbitrarily.

## Component map

| Layer | Location | Role |
|-------|----------|------|
| Swift shim | `virt/arcbox-vz/shim/Sources/ArcBoxVZShim/Usb.swift` | Accessory listener, xHCI config, attach/detach on the VM queue |
| VZ bindings | `virt/arcbox-vz/src/usb.rs` | Rust wrappers: accessory events channel, blocking attach/detach |
| VMM | `virt/arcbox-vmm/src/vmm/darwin.rs` | `VmmConfig.usb`, controller at init, HV rejection |
| Core | `app/arcbox-core/src/usb/` | `UsbManager` registry, selector, guest-path matching |
| Agent | `guest/arcbox-agent/src/agent/linux/usb.rs` | Guest sysfs USB listing (`ListGuestUsbDevices` RPC) |
| API | `rpc/arcbox-protocol/proto/api.proto` (`SystemService`) | `ListUsbDevices` / `AttachUsbDevice` / `DetachUsbDevice` |
| CLI | `app/arcbox-cli/src/commands/usb.rs` | `arcbox usb list\|attach\|detach` |
