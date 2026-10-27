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
        || newer_than_elf(&ebpf_dir.join("Cargo.lock"))
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
            _ => {
                // eBPF build failed — this is expected on non-Linux or without bpf-linker
                // Create a placeholder so the userspace crate still compiles
                eprintln!("cargo:warning=eBPF build skipped (requires Linux + bpf-linker)");
                std::fs::create_dir_all(&target_dir).expect("create eBPF target dir");
                std::fs::write(&elf_path, []).expect("write placeholder eBPF ELF");
            }
        }
    }

    // Expose the ELF path to the userspace crate
    println!("cargo:rustc-env=XDP_EBPF_ELF={}", elf_path.display());
}
