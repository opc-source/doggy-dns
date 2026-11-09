use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn last_modified(dir: &Path) -> Option<SystemTime> {
    let mut newest = dir.metadata().ok()?.modified().ok()?;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let found = if path.is_dir() {
            last_modified(&path)
        } else {
            path.metadata().ok().and_then(|m| m.modified().ok())
        };
        if let Some(time) = found {
            newest = newest.max(time);
        }
    }
    Some(newest)
}

/// Whether the compiled ELF is older than any eBPF input (sources, manifest,
/// lockfile). Without this, a changed source next to an existing ELF would
/// silently embed the previous program. A zero-byte ELF is the placeholder
/// written when no program could be built (a non-Linux target, or a failed
/// nested build) and never counts as up to date.
fn needs_rebuild(elf_path: &Path, ebpf_dir: &Path, abi_dir: &Path) -> bool {
    let Ok(meta) = elf_path.metadata() else {
        return true;
    };
    if meta.len() == 0 {
        return true;
    }
    let elf_time = match meta.modified() {
        Ok(time) => time,
        Err(_) => return true,
    };
    let newer_than_elf = |path: &Path| last_modified(path).is_some_and(|t| t > elf_time);
    newer_than_elf(&ebpf_dir.join("src"))
        || newer_than_elf(&ebpf_dir.join("Cargo.toml"))
        // The eBPF program is a member of the `xdp/` workspace, so its
        // lockfile lives one level above the crate directory.
        || newer_than_elf(&ebpf_dir.join("../Cargo.lock"))
        // The map layouts come from this sibling crate.
        || newer_than_elf(&abi_dir.join("src"))
        || newer_than_elf(&abi_dir.join("Cargo.toml"))
        // The nested build takes its release profile (LTO, opt-level, panic
        // strategy) from the `xdp/` workspace manifest and, once the rustup
        // shim's variables are dropped, its compiler from the toolchain files.
        || newer_than_elf(&ebpf_dir.join("../Cargo.toml"))
        || newer_than_elf(&ebpf_dir.join("rust-toolchain.toml"))
        || newer_than_elf(&ebpf_dir.join("../rust-toolchain.toml"))
}

/// Whether `path` holds a real ELF rather than the zero-byte stand-in.
fn elf_is_usable(path: &Path) -> bool {
    path.metadata().is_ok_and(|m| m.len() > 0)
}

/// Keep a previously built ELF and say it may be stale; without one, write the
/// placeholder so the userspace crate still compiles.
fn skip_build(elf_path: &Path, reason: &str) {
    if elf_is_usable(elf_path) {
        println!(
            "cargo:warning=eBPF build not run ({reason}); reusing the existing ELF, \
             which may be stale"
        );
    } else {
        println!("cargo:warning=eBPF build not run ({reason}); embedding the placeholder");
        write_placeholder(elf_path);
    }
}

/// Whether `bpf-linker` can be found. On a Linux host without it the nested
/// build is guaranteed to fail at link time, after compiling `core`/`alloc`;
/// probing first skips that work.
fn bpf_linker_available() -> bool {
    std::process::Command::new("bpf-linker")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Write the zero-byte stand-in that both the loader and the artifact test
/// read as "no program": they only need the path to exist, so a target
/// without the eBPF toolchain still compiles.
fn write_placeholder(elf_path: &Path) {
    if let Some(parent) = elf_path.parent() {
        std::fs::create_dir_all(parent).expect("create eBPF target dir");
    }
    std::fs::write(elf_path, []).expect("write placeholder eBPF ELF");
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ebpf_dir = manifest_dir.join("../../xdp/doggy-dns-xdp-ebpf");
    let abi_dir = ebpf_dir.with_file_name("doggy-dns-xdp-abi");

    // Tell cargo to rerun if any eBPF or shared-ABI source changes. The lines
    // stay unconditional: the non-Linux skip below must still be re-decided
    // when a source it would have built is edited.
    println!("cargo:rerun-if-changed={}", ebpf_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("../Cargo.lock").display()
    );
    // Workspace release profile and the toolchain the nested build resolves.
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("../Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("rust-toolchain.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("../rust-toolchain.toml").display()
    );
    println!("cargo:rerun-if-changed={}", abi_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        abi_dir.join("Cargo.toml").display()
    );

    // Build the eBPF program into this crate's OUT_DIR, never into the `xdp/`
    // workspace's target directory: a build script writes build state under
    // OUT_DIR and nowhere in the source tree. OUT_DIR persists across rebuilds,
    // so the fingerprint below stays stable, and the nested build gets a target
    // directory of its own below it so the two builds cannot collide. Plain
    // `env!` cannot reach either variable: cargo sets them for the script's
    // run, not for its compilation.
    let out_dir =
        PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR for build scripts"));
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("cargo sets CARGO_CFG_TARGET_OS for build scripts");
    let ebpf_target_dir = out_dir.join("ebpf");
    let elf_path = ebpf_target_dir
        .join("bpfel-unknown-none")
        .join("release")
        .join("doggy-dns-xdp-ebpf");

    // The nested build needs Linux and bpf-linker. On any other target it is
    // guaranteed to fail, so skip it before needs_rebuild and write the
    // placeholder unconditionally: there is no ELF there to reuse.
    if target_os != "linux" {
        write_placeholder(&elf_path);
        println!("cargo:warning=eBPF build skipped (target is not Linux)");
    } else if needs_rebuild(&elf_path, &ebpf_dir, &abi_dir) {
        if !bpf_linker_available() {
            skip_build(&elf_path, "bpf-linker is not on PATH");
        } else {
            let mut command = std::process::Command::new("cargo");
            command
                .current_dir(&ebpf_dir)
                .args([
                    "build",
                    "-Z",
                    "build-std=core,alloc",
                    "--release",
                    "--target",
                    "bpfel-unknown-none",
                ])
                .arg("--target-dir")
                .arg(&ebpf_target_dir);
            // Run the nested build on the toolchain the `xdp/` workspace
            // declares, not the one that is building this crate. Cargo invoked
            // through rustup's shim exports the toolchain it resolved (stable,
            // from the root toolchain file) as RUSTUP_TOOLCHAIN, which outranks
            // a toolchain file, and it hands the script RUSTC/RUSTDOC for that
            // same toolchain. Left in place either one makes the nested build
            // reject `-Z build-std` or look for `rust-src` in the stable
            // sysroot. Dropping all three lets the nested cargo resolve nightly
            // from `xdp/rust-toolchain.toml` and use that toolchain's rustc.
            // Only the shim sets RUSTUP_TOOLCHAIN; a pinned or wrapped
            // compiler that a caller exported must keep its selection.
            if std::env::var_os("RUSTUP_TOOLCHAIN").is_some() {
                command
                    .env_remove("RUSTUP_TOOLCHAIN")
                    .env_remove("RUSTC")
                    .env_remove("RUSTDOC");
            }

            match command.status() {
                Ok(s) if s.success() && elf_is_usable(&elf_path) => {}
                Ok(s) if s.success() => skip_build(
                    &elf_path,
                    "the nested build reported success but wrote no artifact",
                ),
                other => {
                    // A failed nested invocation (e.g. blocked by an outer
                    // cargo lock) must not destroy a previously built ELF: keep
                    // it and let the next build decide. Only a missing or
                    // placeholder artifact gets a stand-in, so the userspace
                    // crate still compiles on hosts without the eBPF toolchain.
                    let reason = match other {
                        Ok(s) => format!("nested build failed: exit status {s}"),
                        Err(e) => format!("could not run cargo: {e}"),
                    };
                    skip_build(&elf_path, &reason);
                }
            }
        }
    }

    // Expose the ELF path to the userspace crate
    println!("cargo:rustc-env=XDP_EBPF_ELF={}", elf_path.display());
}
