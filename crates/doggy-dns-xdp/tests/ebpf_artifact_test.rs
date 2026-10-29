//! Guard for the eBPF artifact the loader embeds.
//!
//! The kernel-side log level is applied by overriding the program's
//! `AYA_LOG_LEVEL` global before load. If a change drops `aya-log-ebpf` (or
//! the symbol is renamed), the loader's override silently becomes a no-op and
//! `AYA_LOG_LEVEL=off` stops taking effect. Inspecting the embedded ELF turns
//! that regression into a compile-and-test failure instead of a runtime
//! surprise.

#[test]
fn embedded_elf_carries_the_log_level_symbol() {
    let elf = std::fs::read(env!("XDP_EBPF_ELF")).expect("read embedded eBPF ELF");

    // A zero-byte file is the placeholder build.rs writes when the eBPF
    // toolchain is unavailable (for example on a macOS dev host), so there is
    // nothing to inspect here.
    if elf.is_empty() {
        eprintln!("skipping: eBPF ELF is a placeholder for a host without the toolchain");
        return;
    }

    let symbol = b"AYA_LOG_LEVEL";
    assert!(
        elf.windows(symbol.len()).any(|window| window == symbol),
        "embedded eBPF ELF has no AYA_LOG_LEVEL symbol; kernel log levels cannot be applied"
    );
}
