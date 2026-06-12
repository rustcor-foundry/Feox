//! Builds the riscv64 U-mode app(s) the kernel embeds (milestone 16).
//!
//! For riscv64 kernel builds only: drives a nested `cargo build` of
//! `apps/feox-hello` (excluded from the repo workspace; it carries its own
//! linker script and target flags in `apps/feox-hello/.cargo/config.toml`)
//! into a target dir under OUT_DIR, and exports the resulting ELF's path as
//! `FEOX_HELLO_ELF` for `include_bytes!(env!(...))` in `arch/riscv64/elf.rs`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("riscv64") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let apps = manifest.join("..").join("..").join("apps");
    let target_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("feox-apps");
    println!("cargo:rerun-if-changed={}", apps.display());

    // The apps' rustflags are set via env because env REPLACES config-level
    // rustflags, whereas config files MERGE: the repo-level
    // [target.riscv64gc-unknown-none-elf] rustflags (the KERNEL linker
    // script) would otherwise be joined with the apps' own flags.
    // -Tlink.ld resolves relative to the linker cwd, each app dir.
    let app_rustflags = "-Clink-arg=-Tlink.ld -Clink-arg=-zmax-page-size=4096 \
                         -Crelocation-model=static -Ccode-model=medium";
    for app in ["feox-hello", "feox-pingpong", "feox-netapp"] {
        let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(apps.join(app))
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env("RUSTFLAGS", app_rustflags)
            .args([
                "build",
                "--release",
                "--target",
                "riscv64gc-unknown-none-elf",
                "--target-dir",
            ])
            .arg(&target_dir)
            .status()
            .unwrap_or_else(|error| panic!("failed to spawn cargo for apps/{app}: {error}"));
        assert!(status.success(), "apps/{app} build failed");

        let elf = target_dir
            .join("riscv64gc-unknown-none-elf")
            .join("release")
            .join(app);
        let var = format!("{}_ELF", app.to_uppercase().replace('-', "_"));
        println!("cargo:rustc-env={}={}", var, elf.display());
    }
}
