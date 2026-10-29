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
/// written by a failed build (e.g. macOS host) and never counts as up to date.
fn needs_rebuild(elf_path: &Path, ebpf_dir: &Path) -> bool {
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
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ebpf_dir = manifest_dir.join("../../xdp/doggy-dns-xdp-ebpf");

    // Tell cargo to rerun if any eBPF source changes
    println!("cargo:rerun-if-changed={}", ebpf_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("../Cargo.lock").display()
    );

    // Build the eBPF program
    let target_dir = manifest_dir.join("../../xdp/target/bpfel-unknown-none/release");
    let elf_path = target_dir.join("doggy-dns-xdp-ebpf");

    if needs_rebuild(&elf_path, &ebpf_dir) {
        let status = std::process::Command::new("cargo")
            .current_dir(&ebpf_dir)
            .args([
                "build",
                "-Z",
                "build-std=core,alloc",
                "--release",
                "--target",
                "bpfel-unknown-none",
            ])
            .status();

        match status {
            Ok(s) if s.success() => {}
            other => {
                // A failed nested invocation (e.g. blocked by an outer cargo
                // lock) must not destroy a previously built ELF: keep it and
                // let the next build decide. Only a missing or placeholder
                // artifact gets a stand-in, so the userspace crate still
                // compiles on hosts without the eBPF toolchain.
                let reason = match other {
                    Ok(s) => format!("exit status {s}"),
                    Err(e) => format!("could not run cargo: {e}"),
                };
                let usable = elf_path.metadata().is_ok_and(|m| m.len() > 0);
                if usable {
                    // The embedded program may not match the sources just
                    // edited, so say so instead of failing silently.
                    eprintln!(
                        "cargo:warning=eBPF build failed ({reason}); reusing the existing ELF, \
                         which may be stale"
                    );
                } else {
                    eprintln!(
                        "cargo:warning=eBPF build skipped (requires Linux + bpf-linker): {reason}"
                    );
                    std::fs::create_dir_all(&target_dir).expect("create eBPF target dir");
                    std::fs::write(&elf_path, []).expect("write placeholder eBPF ELF");
                }
            }
        }
    }

    // Expose the ELF path to the userspace crate
    println!("cargo:rustc-env=XDP_EBPF_ELF={}", elf_path.display());
}
