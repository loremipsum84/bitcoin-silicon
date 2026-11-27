use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/metal.m");
    println!("cargo:rerun-if-changed=native/sha256.metal");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("metal.o");
    let archive = out.join("libsilicon_metal.a");
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("Unknown macOS architecture: {other}"),
    };
    assert!(
        Command::new("xcrun")
            .args([
                "--sdk",
                "macosx",
                "clang",
                "-arch",
                arch,
                "-O2",
                "-Wall",
                "-Wextra",
                "-fobjc-arc",
                "-c",
                "native/metal.m",
                "-o"
            ])
            .arg(&object)
            .status()
            .unwrap()
            .success(),
        "Failed to compile the Metal bridge"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(&archive)
            .arg(&object)
            .status()
            .unwrap()
            .success(),
        "Failed to archive the Metal bridge"
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=silicon_metal");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Metal");
}
