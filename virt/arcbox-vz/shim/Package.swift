// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "ArcBoxVZShim",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "ArcBoxVZShim", type: .static, targets: ["ArcBoxVZShim"])
    ],
    targets: [
        .target(
            name: "ArcBoxVZShim",
            path: "Sources/ArcBoxVZShim",
            swiftSettings: [
                .swiftLanguageMode(.v5)
            ],
            linkerSettings: [
                // Recorded as autolink metadata; the authoritative link args
                // are emitted by arcbox-vz's build.rs (explicit -framework).
                // AccessoryAccess (macOS 27 USB passthrough) is not listed:
                // its autolink hint comes from Usb.swift's conditional
                // `import AccessoryAccess`, and build.rs links it only when
                // the SDK supports it.
                .linkedFramework("Virtualization")
            ]
        )
    ]
)
