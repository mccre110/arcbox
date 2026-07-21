//! Builds the ArcBoxVZShim Swift static library and links it (plus the Swift
//! runtime stubs and Virtualization.framework) into this crate.
//!
//! Non-macOS targets get no shim: the crate root is `#![cfg(target_os =
//! "macos")]`, so the library is empty there and this script must not touch
//! the Swift toolchain (docs.rs / Linux workspace builds).

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// The system `xcrun`, isolated from nix/devenv toolchain overrides.
///
/// The devenv shell pins `SDKROOT`/`DEVELOPER_DIR` to a nix-store Apple SDK
/// for the C/Rust side (arcbox-vmm's hv_gic_* linking) and shadows `xcrun`
/// on PATH with xcbuild's fake. SwiftPM refuses an SDK whose Swift stdlib
/// was built by a different compiler than the active toolchain, so Swift
/// invocations must go through the real `/usr/bin/xcrun` and resolve the
/// SDK from the toolchain's own defaults.
fn xcrun() -> Command {
    let mut cmd = Command::new("/usr/bin/xcrun");
    cmd.env_remove("SDKROOT").env_remove("DEVELOPER_DIR");
    cmd
}

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    println!("cargo:rerun-if-changed=shim/Package.swift");
    println!("cargo:rerun-if-changed=shim/Sources");
    // USB passthrough support depends on which SDK the final linker uses.
    println!("cargo:rerun-if-env-changed=SDKROOT");

    let have_swiftc = xcrun()
        .args(["--find", "swiftc"])
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        have_swiftc,
        "arcbox-vz: `swiftc` not found. The crate builds its Swift shim (ArcBoxVZShim) with the \
         Xcode Command Line Tools, a documented prerequisite (CONTRIBUTING.md -> Prerequisites). \
         Install with `xcode-select --install`."
    );

    let config = if env::var("PROFILE").as_deref() == Ok("release") {
        "release"
    } else {
        "debug"
    };
    let triple = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64-apple-macosx13.0",
        Ok("x86_64") => "x86_64-apple-macosx13.0",
        other => panic!("arcbox-vz: unsupported macOS arch {other:?}"),
    };

    let pkg = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("shim");
    let scratch = PathBuf::from(env::var("OUT_DIR").unwrap()).join("swiftpm-scratch");

    // The SDK the Swift shim compiles against (toolchain default — SDKROOT is
    // scrubbed for Swift invocations, see xcrun()).
    let swift_sdk = {
        let sdk = xcrun()
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .expect("failed to spawn `xcrun --show-sdk-path`");
        assert!(sdk.status.success(), "xcrun --show-sdk-path failed");
        String::from_utf8(sdk.stdout).unwrap().trim().to_string()
    };
    // The SDK the FINAL linker resolves .tbd stubs against (SDKROOT when set
    // — e.g. the devenv nix SDK — else the toolchain default above).
    let linker_sdk = env::var("SDKROOT")
        .ok()
        .filter(|s| PathBuf::from(s).is_dir())
        .unwrap_or_else(|| swift_sdk.clone());

    // USB passthrough needs the macOS 27 SDK on BOTH sides: the Swift compile
    // (AccessoryAccess + VZUSBPassthrough* declarations — canImport gates
    // this) and the final link (the older Virtualization.tbd lacks the new
    // symbols). AccessoryAccess.framework's presence is the proxy for a
    // macOS 27+ SDK. When the compile SDK has it but the linker SDK does not,
    // -D ARCBOX_USB_DISABLED compiles the USB entry points to "unsupported"
    // stubs so the link stays resolvable (see Usb.swift).
    let has_usb_framework = |sdk: &str| {
        PathBuf::from(sdk)
            .join("System/Library/Frameworks/AccessoryAccess.framework")
            .is_dir()
    };
    let usb_enabled = has_usb_framework(&swift_sdk) && has_usb_framework(&linker_sdk);

    // Shared by `build` and `--show-bin-path` so both resolve the same layout.
    let mut common = vec![
        "--package-path",
        pkg.to_str().unwrap(),
        "--scratch-path",
        scratch.to_str().unwrap(),
        "-c",
        config,
        "--triple",
        triple,
        "--product",
        "ArcBoxVZShim",
    ];
    if has_usb_framework(&swift_sdk) && !usb_enabled {
        common.extend(["-Xswiftc", "-DARCBOX_USB_DISABLED"]);
    }
    let common = common;

    let out = xcrun()
        .args(["swift", "build"])
        .args(&common)
        .output()
        .expect("failed to spawn `xcrun swift build`");
    assert!(
        out.status.success(),
        "swift build failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let bin = xcrun()
        .args(["swift", "build"])
        .args(&common)
        .arg("--show-bin-path")
        .output()
        .expect("failed to spawn `xcrun swift build --show-bin-path`");
    assert!(bin.status.success(), "swift build --show-bin-path failed");
    let bin_path = PathBuf::from(String::from_utf8(bin.stdout).unwrap().trim());
    assert!(
        bin_path.join("libArcBoxVZShim.a").exists(),
        "libArcBoxVZShim.a missing in {}",
        bin_path.display()
    );

    // The final link is driven by the environment's `cc` (the nix clang
    // wrapper inside the devenv shell), which resolves .tbd stubs against
    // ITS SDK. The Swift runtime search path must therefore come from the
    // same SDK the linker uses: honor SDKROOT when set, else the toolchain
    // default. Mixing 6.3-compiled Swift objects with another SDK's runtime
    // stubs is sound — the Swift ABI is stable and the absolute-install-name
    // dylibs (libswiftCore etc.) resolve from the dyld shared cache at
    // runtime. NOTE: libswift_Concurrency's install name is `@rpath/...`, so
    // it additionally needs an rpath to /usr/lib/swift on the FINAL binary —
    // provided workspace-wide in `.cargo/config.toml` (a build-script link
    // arg here would not propagate to downstream binaries).
    let swift_stub_dir = format!("{linker_sdk}/usr/lib/swift");

    println!("cargo:rustc-link-search=native={}", bin_path.display());
    println!("cargo:rustc-link-lib=static=ArcBoxVZShim");
    println!("cargo:rustc-link-search=native={swift_stub_dir}");
    // ld does not honor the archive's LC_LINKER_OPTION autolink hints when
    // rustc drives the link, so forward the Swift runtime libs explicitly.
    for lib in autolink_swift_libs(&bin_path.join("libArcBoxVZShim.a")) {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    // Explicit rather than autolink: deterministic, and it replaces the old
    // runtime dlopen of Virtualization.framework.
    println!("cargo:rustc-link-lib=framework=Virtualization");
    println!("cargo:rustc-link-lib=framework=Foundation");
    if usb_enabled {
        // Every AccessoryAccess reference sits behind #available(macOS 27)
        // in the shim, so ld marks them all weak_import and auto-weakens the
        // framework load command (LC_LOAD_WEAK_DYLIB) — the binary still
        // launches on the macOS 13 deployment floor. Verify with
        // `otool -L <binary> | grep AccessoryAccess` (must say "weak").
        println!("cargo:rustc-link-lib=framework=AccessoryAccess");
    }
}

/// Swift runtime libraries the shim's objects autolink (`LC_LINKER_OPTION`).
///
/// Only `-lswift*` and `-lobjc` are forwarded: the SDK's `usr/lib/swift`
/// stubs resolve them. `swiftCompatibility*` shims are skipped — they are
/// static archives in the toolchain (not the SDK) backfilling runtimes older
/// than our macOS 13 deployment floor, so none of their symbols are
/// referenced.
fn autolink_swift_libs(archive: &std::path::Path) -> Vec<String> {
    let out = xcrun()
        .args(["otool", "-l"])
        .arg(archive)
        .output()
        .expect("failed to spawn `xcrun otool -l`");
    assert!(
        out.status.success(),
        "otool -l failed on {}",
        archive.display()
    );

    let mut libs: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let opt = line.trim().strip_prefix("string #")?.split_once(' ')?.1;
            let lib = opt.strip_prefix("-l")?;
            ((lib.starts_with("swift") && !lib.starts_with("swiftCompatibility")) || lib == "objc")
                .then(|| lib.to_string())
        })
        .collect();
    libs.sort();
    libs.dedup();
    assert!(
        libs.iter().any(|l| l == "swiftCore"),
        "no swiftCore autolink entry found in {} — otool parse broke?",
        archive.display()
    );
    libs
}
