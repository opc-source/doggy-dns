use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ebpf_dir = manifest_dir.join("../../xdp/doggy-dns-xdp-ebpf");

    // Tell cargo to rerun if eBPF source changes
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("src/main.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ebpf_dir.join("Cargo.toml").display()
    );

    // Build the eBPF program
    let target_dir = manifest_dir.join("../../xdp/target/bpfel-unknown-none/release");
    let elf_path = target_dir.join("doggy-dns-xdp-ebpf");

    if !elf_path.exists() {
        // Attempt to build the eBPF program
        let status = std::process::Command::new("cargo")
            .current_dir(&ebpf_dir)
            .args(["build", "--release", "--target", "bpfel-unknown-none"])
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
