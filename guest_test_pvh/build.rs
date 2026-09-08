// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build configuration for the fixed-address Xen PVH test guest.

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");

    // xtask-fmt allow-target-arch oneoff-guest-arch-impl
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    if arch == "x86_64" && target_os == "none" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg=-T{manifest_dir}/linker.ld");
        println!("cargo:rustc-link-arg=-no-pie");
        println!("cargo:rustc-link-arg=--build-id=none");
    }
}
