// USB passthrough (mirrors src/usb.rs and src/device/usb.rs).
//
// Built on the macOS 27 Accessory Access framework: the user grants USB
// accessories to the app via the system Accessory Access UI, the registered
// listener receives connect/disconnect events, and granted accessories are
// hot-plugged into a running VM as VZUSBPassthroughDevice on a USB (XHCI)
// controller.
//
// SDK adaptivity: AccessoryAccess (and the VZUSBPassthrough* types) exist
// only in the macOS 27 SDK. On older SDKs — or when build.rs detects that
// the final linker's SDK lacks the framework and passes -D
// ARCBOX_USB_DISABLED — every entry point compiles to a stub that reports
// "not supported", so the exported symbol set (and link_coverage) never
// changes.
//
// Callback convention note: ABXUsbEventCallback is RECURRING — it fires once
// per accessory event for the lifetime of the registration, unlike the
// exactly-once completion callbacks. The Rust side forwards events into a
// channel and never frees the event ctx (listener registration is
// process-lifetime).

import Virtualization

#if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
    import AccessoryAccess
    import IOKit
#endif

/// Recurring USB accessory event callback.
///
/// Fires once per connect/disconnect event, from the accessory manager's
/// internal serial queue. On connect, `accessory` is a +1 handle owned by
/// the receiver; on disconnect it is nil (the registry ID identifies the
/// accessory). `name` and `serial` are strdup'd (nullable) and freed by the
/// receiver.
public typealias ABXUsbEventCallback =
    @convention(c) (
        UnsafeMutableRawPointer?,  // ctx
        UnsafeMutableRawPointer?,  // accessory (+1 on connect, nil on disconnect)
        UInt64,  // IORegistry ID
        UInt16,  // vendor ID
        UInt16,  // product ID
        UnsafeMutablePointer<CChar>?,  // product name (nullable)
        UnsafeMutablePointer<CChar>?,  // serial number (nullable)
        Bool  // connected
    ) -> Void

/// Pairs a runtime USB controller with the owning VM's serial queue (VZ
/// device objects are queue-affine).
@available(macOS 15.0, *)
final class ABXUsbControllerBox {
    let controller: VZUSBController
    let queue: DispatchQueue

    init(controller: VZUSBController, queue: DispatchQueue) {
        self.controller = controller
        self.queue = queue
    }
}

/// Pairs an attached USB device with the owning VM's serial queue.
@available(macOS 15.0, *)
final class ABXUsbDeviceBox {
    let device: any VZUSBDevice
    let queue: DispatchQueue

    init(device: any VZUSBDevice, queue: DispatchQueue) {
        self.device = device
        self.queue = queue
    }
}

func usbSupported() -> Bool {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            return true
        }
    #endif
    return false
}

func usbXhciConfigNew(
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> UnsafeMutableRawPointer? {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            return abxRetainedHandle(VZXHCIControllerConfiguration())
        }
    #endif
    errorOut?.pointee = abxUsbUnsupported()
    return nil
}

func usbManagerRegister(
    _ eventCtx: UnsafeMutableRawPointer?,
    _ eventCallback: @escaping ABXUsbEventCallback,
    _ completionCtx: UnsafeMutableRawPointer?,
    _ completion: @escaping ABXStateCallback
) {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            let listener = ABXUsbListener(ctx: eventCtx, callback: eventCallback)
            AAUSBAccessoryManager.shared.registerListener(listener, matchingCriteria: []) {
                accessories, error in
                if let error {
                    completion(completionCtx, abxErrorString(error))
                    return
                }
                // The manager's retain policy for listeners is unspecified;
                // keep our own strong reference for the process lifetime.
                ABXUsbListenerRegistry.shared.keep(listener)
                for accessory in accessories {
                    listener.deliver(accessory, connected: true)
                }
                completion(completionCtx, nil)
            }
            return
        }
    #endif
    completion(completionCtx, abxUsbUnsupported())
}

func vmUsbControllerCount(_ box: UnsafeMutableRawPointer) -> UInt64 {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            let vmBox = abxBorrow(box, as: ABXVMBox.self)
            return vmBox.queue.sync { UInt64(vmBox.vm.usbControllers.count) }
        }
    #endif
    return 0
}

func vmUsbControllerAt(
    _ box: UnsafeMutableRawPointer, _ index: UInt64
) -> UnsafeMutableRawPointer? {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            let vmBox = abxBorrow(box, as: ABXVMBox.self)
            return vmBox.queue.sync {
                guard let controller = vmBox.vm.usbControllers[safe: Int(index)] else {
                    return nil
                }
                return abxRetainedHandle(
                    ABXUsbControllerBox(controller: controller, queue: vmBox.queue))
            }
        }
    #endif
    return nil
}

func usbControllerAttach(
    _ box: UnsafeMutableRawPointer,
    _ accessory: UnsafeMutableRawPointer,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @escaping ABXObjectCallback
) {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            let controllerBox = abxBorrow(box, as: ABXUsbControllerBox.self)
            let usbAccessory = abxBorrow(accessory, as: AAUSBAccessory.self)
            controllerBox.queue.async {
                do {
                    let configuration = VZUSBPassthroughDeviceConfiguration(device: usbAccessory)
                    let device = try VZUSBPassthroughDevice(configuration: configuration)
                    controllerBox.controller.attach(device: device) { error in
                        if let error {
                            callback(ctx, nil, abxErrorString(error))
                        } else {
                            let deviceBox = ABXUsbDeviceBox(
                                device: device, queue: controllerBox.queue)
                            callback(ctx, abxRetainedHandle(deviceBox), nil)
                        }
                    }
                } catch {
                    callback(ctx, nil, abxErrorString(error))
                }
            }
            return
        }
    #endif
    callback(ctx, nil, abxUsbUnsupported())
}

func usbControllerDetach(
    _ box: UnsafeMutableRawPointer,
    _ device: UnsafeMutableRawPointer,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @escaping ABXStateCallback
) {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        if #available(macOS 27.0, *) {
            let controllerBox = abxBorrow(box, as: ABXUsbControllerBox.self)
            let deviceBox = abxBorrow(device, as: ABXUsbDeviceBox.self)
            controllerBox.queue.async {
                controllerBox.controller.detach(device: deviceBox.device) { error in
                    if let error {
                        callback(ctx, abxErrorString(error))
                    } else {
                        callback(ctx, nil)
                    }
                }
            }
            return
        }
    #endif
    callback(ctx, abxUsbUnsupported())
}

private func abxUsbUnsupported() -> UnsafeMutablePointer<CChar>? {
    #if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED
        return abxStrdup("USB passthrough requires macOS 27 or newer")
    #else
        return abxStrdup(
            "USB passthrough support was not compiled in (macOS 27 SDK required at build time)")
    #endif
}

#if canImport(AccessoryAccess) && !ARCBOX_USB_DISABLED

    /// Forwards Accessory Access events to the recurring C callback.
    ///
    /// The manager invokes listener methods on its internal serial queue, so
    /// deliveries never overlap. `@unchecked Sendable`: the stored context
    /// pointer is owned by the Rust side for the process lifetime and the C
    /// callback is thread-safe by contract.
    @available(macOS 27.0, *)
    final class ABXUsbListener: NSObject, AAUSBAccessoryListener, @unchecked Sendable {
        private let ctx: UnsafeMutableRawPointer?
        private let callback: ABXUsbEventCallback

        init(ctx: UnsafeMutableRawPointer?, callback: @escaping ABXUsbEventCallback) {
            self.ctx = ctx
            self.callback = callback
        }

        func usbAccessoryDidConnect(_ usbAccessory: AAUSBAccessory) {
            deliver(usbAccessory, connected: true)
        }

        func usbAccessoryDidDisconnect(_ usbAccessory: AAUSBAccessory) {
            deliver(usbAccessory, connected: false)
        }

        func deliver(_ accessory: AAUSBAccessory, connected: Bool) {
            let (vendorID, productID) = usbDescriptorIDs(accessory.deviceDescriptorData)
            // IORegistry lookups fail for disconnected devices — nils are fine.
            let name = usbRegistryString(accessory.registryID, "USB Product Name")
            let serial = usbRegistryString(accessory.registryID, "USB Serial Number")
            let handle: UnsafeMutableRawPointer? = connected ? abxRetainedHandle(accessory) : nil
            callback(
                ctx, handle, accessory.registryID, vendorID, productID,
                name.flatMap(abxStrdup), serial.flatMap(abxStrdup), connected)
        }
    }

    /// Strong references to registered listeners (process-lifetime).
    @available(macOS 27.0, *)
    private final class ABXUsbListenerRegistry {
        static let shared = ABXUsbListenerRegistry()
        private let lock = NSLock()
        private var listeners: [ABXUsbListener] = []

        func keep(_ listener: ABXUsbListener) {
            lock.lock()
            listeners.append(listener)
            lock.unlock()
        }
    }

    /// Extracts idVendor / idProduct from a standard USB device descriptor
    /// (little-endian u16s at offsets 8 and 10).
    private func usbDescriptorIDs(_ data: Data) -> (UInt16, UInt16) {
        guard data.count >= 12 else { return (0, 0) }
        let vendor = UInt16(data[8]) | (UInt16(data[9]) << 8)
        let product = UInt16(data[10]) | (UInt16(data[11]) << 8)
        return (vendor, product)
    }

    /// Reads a string property from the accessory's IORegistry entry.
    private func usbRegistryString(_ registryID: UInt64, _ key: String) -> String? {
        let service = IOServiceGetMatchingService(
            kIOMainPortDefault, IORegistryEntryIDMatching(registryID))
        guard service != 0 else { return nil }
        defer { IOObjectRelease(service) }
        let value = IORegistryEntryCreateCFProperty(service, key as CFString, kCFAllocatorDefault, 0)
        return value?.takeRetainedValue() as? String
    }

#endif
