use std::{env, path::PathBuf};

fn main() {
    for name in ["launcher.c", "policy.h"] {
        println!("cargo:rerun-if-changed=native/{name}");
    }
    println!("cargo:rustc-check-cfg=cfg(runbay_native_linux)");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || !matches!(
            env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
            Ok("x86_64" | "aarch64")
        )
    {
        return;
    }
    println!("cargo:rustc-cfg=runbay_native_linux");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("runbay-landlock");
    // A static launcher enters its namespaces before any worker-controlled dynamic loader runs.
    let status = cc::Build::new()
        .get_compiler()
        .to_command()
        .args([
            "-std=gnu11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-static",
            "native/launcher.c",
            "-o",
        ])
        .arg(&out)
        .status()
        .expect("could not start the target C compiler");
    assert!(
        status.success(),
        "building the Linux sandbox launcher failed"
    );
}
