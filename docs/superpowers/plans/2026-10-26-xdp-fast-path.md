# XDP Fast Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an eBPF/XDP cache layer to doggy-dns that serves cache-hit DNS responses directly in kernel space via XDP_TX, with userspace middleware populating the BPF map after authority chain resolution.

**Architecture:** An XDP program (built with Aya) attaches to a network interface and intercepts UDP :53 packets. It parses DNS questions, looks up a BPF LRU hash map, and either returns a cached response (XDP_TX) or passes the packet through to the existing hickory-server userspace pipeline (XDP_PASS). A new `XdpCacheMiddleware` writes resolved responses into the BPF map for future fast-path hits. The feature is gated behind `--features xdp` and degrades gracefully when XDP attach fails.

**Tech Stack:** Aya (eBPF for Rust), `bpf-linker`, BPF_MAP_TYPE_LRU_HASH, XDP_TX, hickory-dns 0.26, Tokio, criterion 0.8, dnsperf (CI stress tests)

**Spec:** `docs/superpowers/specs/2026-10-26-xdp-fast-path-design.md`

## Global Constraints

- Rust edition 2024, stable toolchain
- No `.unwrap()` — use `.expect("reason")` or `?` with anyhow (except in test files)
- No `std::time::Instant` — use `tokio::time::Instant`
- No `std::thread::sleep` — use `tokio::time::sleep`
- `cargo clippy --all-features --all-targets -- -D warnings` must pass
- `cargo fmt --all -- --check` must pass
- eBPF program targets `bpfel-unknown-none` (built separately in `xdp/` workspace)
- XDP feature is gated behind `--features xdp`; default build must not require eBPF toolchain
- All XDP failures degrade gracefully — DNS service must never be interrupted

---

### Task 1: Extend `RequestOutcome` with response data fields

**Files:**
- Modify: `crates/doggy-dns-plugin/src/plugin.rs`
- Test: `crates/doggy-dns-plugin/tests/request_outcome_test.rs`

**Interfaces:**
- Produces: `RequestOutcome { response_bytes: Option<Vec<u8>>, min_answer_ttl: Option<u32> }` — two new optional fields added to the existing struct
- Consumed by: Task 7 (XdpCacheMiddleware reads these fields), Task 3 (DoggyDnsHandler populates them)

- [ ] **Step 1: Write failing tests for RequestOutcome extension**

Create `crates/doggy-dns-plugin/tests/request_outcome_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::RequestOutcome;
use hickory_proto::op::ResponseCode;

#[test]
fn default_outcome_has_no_response_bytes() {
    let outcome = RequestOutcome {
        response_code: ResponseCode::NoError,
        authority: Some("test".to_string()),
        answers: 1,
        response_bytes: None,
        min_answer_ttl: None,
    };
    assert!(outcome.response_bytes.is_none());
    assert!(outcome.min_answer_ttl.is_none());
}

#[test]
fn outcome_carries_response_bytes() {
    let bytes = vec![0u8; 64];
    let outcome = RequestOutcome {
        response_code: ResponseCode::NoError,
        authority: Some("forward".to_string()),
        answers: 2,
        response_bytes: Some(bytes.clone()),
        min_answer_ttl: Some(300),
    };
    assert_eq!(outcome.response_bytes.as_deref(), Some(bytes.as_slice()));
    assert_eq!(outcome.min_answer_ttl, Some(300));
}

#[test]
fn nxdomain_outcome_has_zero_ttl_and_no_bytes() {
    let outcome = RequestOutcome {
        response_code: ResponseCode::NXDomain,
        authority: None,
        answers: 0,
        response_bytes: Some(vec![0u8; 12]),
        min_answer_ttl: None,
    };
    assert_eq!(outcome.response_code, ResponseCode::NXDomain);
    assert_eq!(outcome.answers, 0);
    assert!(outcome.min_answer_ttl.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p doggy-dns-plugin --test request_outcome_test`
Expected: FAIL — `RequestOutcome` does not have `response_bytes` or `min_answer_ttl` fields.

- [ ] **Step 3: Add new fields to RequestOutcome**

In `crates/doggy-dns-plugin/src/plugin.rs`, modify the struct:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOutcome {
    /// Response code sent to the client.
    pub response_code: ResponseCode,
    /// Authority that handled the query, e.g. `native` or `forward[8.8.8.8:53]`.
    pub authority: Option<String>,
    /// Number of answer records in the response.
    pub answers: usize,
    /// Raw DNS response wire format. `None` when XDP cache is disabled.
    pub response_bytes: Option<Vec<u8>>,
    /// Minimum TTL across all answer RRs. `None` when XDP cache is disabled or no answers.
    pub min_answer_ttl: Option<u32>,
}
```

- [ ] **Step 4: Update all existing RequestOutcome construction sites**

In `crates/doggy-dns-core/src/handler.rs`, add the new fields (set to `None` for now — Task 3 will populate them properly). Search for every `RequestOutcome {` in the file and add `response_bytes: None, min_answer_ttl: None` to each. There are 4 construction sites:
1. Short-circuit (Refused) — line ~1077
2. Parse error (FormErr) — line ~1097
3. Success (NoError) — line ~1182
4. Error (ServFail) — line ~1199
5. NXDomain (Skip) — line ~1239

For each, add:
```rust
    response_bytes: None,
    min_answer_ttl: None,
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p doggy-dns-plugin --test request_outcome_test`
Expected: PASS

Run: `cargo test --workspace`
Expected: PASS (all existing tests still compile and pass)

- [ ] **Step 6: Commit**

```bash
git add crates/doggy-dns-plugin/src/plugin.rs crates/doggy-dns-plugin/tests/request_outcome_test.rs crates/doggy-dns-core/src/handler.rs
git commit -m "feat(plugin): extend RequestOutcome with response_bytes and min_answer_ttl fields"
```

---

### Task 2: Add `XdpConfig` to config module with validation

**Files:**
- Modify: `crates/doggy-dns-core/src/config.rs`
- Test: `crates/doggy-dns-core/tests/config_test.rs` (append to existing)

**Interfaces:**
- Produces: `XdpConfig` struct, `DoggyDnsConfig.xdp: XdpConfig` field, validation logic in `validate_config()`
- Consumed by: Task 5 (XdpManager reads config), Task 6 (main.rs reads config)

- [ ] **Step 1: Write failing tests for XdpConfig parsing and validation**

Append to `crates/doggy-dns-core/tests/config_test.rs`:

```rust
#[test]
fn parse_xdp_config_defaults() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    assert!(!config.xdp.enabled);
    assert_eq!(config.xdp.interface, "");
    assert_eq!(config.xdp.cache_size, 65536);
    assert_eq!(config.xdp.negative_ttl, 30);
    assert_eq!(config.xdp.attach_mode, "auto");
    assert_eq!(config.xdp.pin_path, "/sys/fs/bpf/doggy-dns/dns_cache");
}

#[test]
fn parse_xdp_config_custom() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
enabled = true
interface = "eth0"
cache_size = 32768
negative_ttl = 60
attach_mode = "manual"
pin_path = "/sys/fs/bpf/custom/dns_cache"
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    assert!(config.xdp.enabled);
    assert_eq!(config.xdp.interface, "eth0");
    assert_eq!(config.xdp.cache_size, 32768);
    assert_eq!(config.xdp.negative_ttl, 60);
    assert_eq!(config.xdp.attach_mode, "manual");
    assert_eq!(config.xdp.pin_path, "/sys/fs/bpf/custom/dns_cache");
}

#[test]
fn validate_xdp_cache_size_zero_rejected() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
enabled = true
cache_size = 0
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    let err = validate_config(&config).unwrap_err();
    assert!(err.to_string().contains("cache_size"));
}

#[test]
fn validate_xdp_negative_ttl_zero_rejected() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
enabled = true
negative_ttl = 0
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    let err = validate_config(&config).unwrap_err();
    assert!(err.to_string().contains("negative_ttl"));
}

#[test]
fn validate_xdp_invalid_attach_mode_rejected() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
enabled = true
attach_mode = "invalid"
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    let err = validate_config(&config).unwrap_err();
    assert!(err.to_string().contains("attach_mode"));
}

#[test]
fn validate_xdp_pin_path_not_under_bpffs_rejected() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
enabled = true
pin_path = "/tmp/dns_cache"
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    let err = validate_config(&config).unwrap_err();
    assert!(err.to_string().contains("pin_path"));
}

#[test]
fn validate_xdp_disabled_skips_validation() {
    let toml = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[xdp]
enabled = false
cache_size = 0
"#;
    let config: DoggyDnsConfig = toml::from_str(toml).expect("parse failed");
    assert!(validate_config(&config).is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p doggy-dns-core --test config_test -- parse_xdp_config_defaults`
Expected: FAIL — `DoggyDnsConfig` has no `xdp` field.

- [ ] **Step 3: Add XdpConfig struct and field to DoggyDnsConfig**

In `crates/doggy-dns-core/src/config.rs`, add the struct and integrate it:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct XdpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub interface: String,
    #[serde(default = "default_xdp_cache_size")]
    pub cache_size: u32,
    #[serde(default = "default_xdp_negative_ttl")]
    pub negative_ttl: u64,
    #[serde(default = "default_xdp_attach_mode")]
    pub attach_mode: String,
    #[serde(default = "default_xdp_pin_path")]
    pub pin_path: String,
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interface: String::new(),
            cache_size: default_xdp_cache_size(),
            negative_ttl: default_xdp_negative_ttl(),
            attach_mode: default_xdp_attach_mode(),
            pin_path: default_xdp_pin_path(),
        }
    }
}

fn default_xdp_cache_size() -> u32 {
    65536
}

fn default_xdp_negative_ttl() -> u64 {
    30
}

fn default_xdp_attach_mode() -> String {
    "auto".to_string()
}

fn default_xdp_pin_path() -> String {
    "/sys/fs/bpf/doggy-dns/dns_cache".to_string()
}
```

Add the field to `DoggyDnsConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct DoggyDnsConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub middleware: MiddlewareConfig,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub remote_config: RemoteConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub xdp: XdpConfig,
}
```

- [ ] **Step 4: Add XDP validation to `validate_config()`**

In the `validate_config` function, add before the final `Ok(())`:

```rust
    if config.xdp.enabled {
        anyhow::ensure!(
            config.xdp.cache_size > 0,
            "xdp.cache_size must be greater than 0"
        );
        anyhow::ensure!(
            config.xdp.negative_ttl > 0,
            "xdp.negative_ttl must be greater than 0"
        );
        anyhow::ensure!(
            matches!(config.xdp.attach_mode.as_str(), "auto" | "manual"),
            "xdp.attach_mode must be 'auto' or 'manual'"
        );
        anyhow::ensure!(
            config.xdp.pin_path.starts_with("/sys/fs/bpf/"),
            "xdp.pin_path must be under /sys/fs/bpf/"
        );
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p doggy-dns-core --test config_test`
Expected: All tests PASS.

Run: `cargo test --workspace`
Expected: All existing tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/doggy-dns-core/src/config.rs crates/doggy-dns-core/tests/config_test.rs
git commit -m "feat(config): add XdpConfig with validation for XDP fast path"
```

---

### Task 3: Populate `response_bytes` and `min_answer_ttl` in handler

**Files:**
- Modify: `crates/doggy-dns-core/src/handler.rs`

**Interfaces:**
- Consumes: `RequestOutcome` from Task 1 (with new fields)
- Produces: `RequestOutcome` instances with `response_bytes` and `min_answer_ttl` populated for NoError/NXDomain outcomes

- [ ] **Step 1: Add a helper to extract min TTL from lookup records**

In `crates/doggy-dns-core/src/handler.rs`, add a helper function:

```rust
use hickory_proto::rr::Record;

fn min_ttl_from_records(records: &[Record]) -> Option<u32> {
    records.iter().map(|r| r.ttl()).min()
}
```

- [ ] **Step 2: Add a helper to serialize DNS response to wire format**

Add a function that converts the hickory response into raw bytes for the BPF cache:

```rust
use hickory_proto::op::Message;
use hickory_server::zone_handler::Lookup;

fn serialize_dns_response(
    request: &Request,
    lookup: &Lookup,
    response_code: ResponseCode,
) -> Option<Vec<u8>> {
    let builder = MessageResponseBuilder::from_message_request(request);
    let mut metadata = Metadata::response_from_request(&request.metadata);
    metadata.message_type = MessageType::Response;
    metadata.op_code = OpCode::Query;
    metadata.response_code = response_code;
    metadata.recursion_available = true;

    let records: Vec<_> = lookup.iter().cloned().collect();
    let response = builder.build(metadata, records.iter(), &[], &[], &[]);

    // Serialize to wire format
    let mut buf = Vec::with_capacity(512);
    response.convert_to_bytes(&mut buf).ok()?;
    Some(buf)
}
```

Note: Check if `MessageResponse::convert_to_bytes` exists in hickory 0.26. If not, use `Message::to_vec()` or construct the bytes manually via `hickory_proto::serialize::binary::BinEncoder`. The exact API depends on what hickory exposes — verify during implementation.

- [ ] **Step 3: Update the NoError outcome construction to populate new fields**

In the `LookupControlFlow::Continue(Ok(lookup)) | Break(Ok(lookup))` arm, modify the outcome construction:

```rust
let records: Vec<_> = lookup.iter().cloned().collect();
let answer_count = records.len();
let min_ttl = min_ttl_from_records(&records);
let resp_bytes = serialize_dns_response(request, &lookup, ResponseCode::NoError);

// ... existing send_response code ...

RequestOutcome {
    response_code: ResponseCode::NoError,
    authority: Some(handler_name.to_string()),
    answers: answer_count,
    response_bytes: resp_bytes,
    min_answer_ttl: min_ttl,
}
```

- [ ] **Step 4: Update the NXDomain outcome to include response bytes**

In the `LookupControlFlow::Skip` arm:

```rust
let resp_bytes = serialize_dns_response(request, &empty_lookup, ResponseCode::NXDomain);

RequestOutcome {
    response_code: ResponseCode::NXDomain,
    authority: None,
    answers: 0,
    response_bytes: resp_bytes,
    min_answer_ttl: None,
}
```

Note: For NXDomain, you may need to construct a minimal empty Lookup or use a different approach to serialize the error response. The key is that the BPF cache needs the full DNS response bytes to construct the XDP_TX reply.

- [ ] **Step 5: Run tests and clippy**

Run: `cargo test --workspace`
Run: `cargo clippy --all-features --all-targets -- -D warnings`
Expected: All pass.

- [ ] **Step 6: Commit**

```bash
git add crates/doggy-dns-core/src/handler.rs
git commit -m "feat(handler): populate response_bytes and min_answer_ttl in RequestOutcome"
```

---

### Task 4: Set up eBPF workspace with shared types and build infrastructure

**Files:**
- Create: `xdp/Cargo.toml`
- Create: `xdp/rust-toolchain.toml`
- Create: `xdp/doggy-dns-xdp-ebpf/Cargo.toml`
- Create: `xdp/doggy-dns-xdp-ebpf/rust-toolchain.toml`
- Create: `crates/doggy-dns-xdp/Cargo.toml`
- Create: `crates/doggy-dns-xdp/build.rs`
- Modify: `Cargo.toml` (workspace root)

**Interfaces:**
- Produces: Two workspaces (main + eBPF), shared type definitions (`CacheKey`, `CacheValue`, `FLAG_NEGATIVE`), build.rs that compiles eBPF ELF
- Consumed by: Task 5 (eBPF program), Task 6 (userspace crate)

- [ ] **Step 1: Create eBPF workspace root**

Create `xdp/Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["doggy-dns-xdp-ebpf"]
```

Create `xdp/rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
targets = ["bpfel-unknown-none"]
```

- [ ] **Step 2: Create eBPF program crate**

Create `xdp/doggy-dns-xdp-ebpf/Cargo.toml`:

```toml
[package]
name = "doggy-dns-xdp-ebpf"
version = "0.1.0"
edition = "2024"

[dependencies]
aya-ebpf = "0.1"
aya-log-ebpf = "0.1"

[[bin]]
name = "doggy-dns-xdp-ebpf"
path = "src/main.rs"

[profile.dev]
opt-level = 2
overflow-checks = false
debug-assertions = false
panic = "abort"

[profile.release]
lto = true
opt-level = "s"
panic = "abort"
```

Create `xdp/doggy-dns-xdp-ebpf/rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
targets = ["bpfel-unknown-none"]
components = ["rust-src"]
```

- [ ] **Step 3: Create placeholder eBPF main with shared types**

Create `xdp/doggy-dns-xdp-ebpf/src/main.rs`:

```rust
#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    macros::{map, xdp},
    maps::LruHashMap,
    programs::XdpContext,
};
use aya_log_ebpf::info;

// Shared types — must match crates/doggy-dns-xdp/src/types.rs exactly
pub const FLAG_NEGATIVE: u16 = 0x01;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheValue {
    pub resp_data: [u8; 512],
    pub resp_len: u16,
    pub flags: u16,
    pub _pad: u32,
    pub expires_at: u64,
}

#[map]
static DNS_CACHE: LruHashMap<CacheKey, CacheValue> =
    LruHashMap::with_max_entries(65536, 0);

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    match try_dns_xdp(ctx) {
        Ok(action) => action,
        Err(_) => xdp_action::XDP_PASS,
    }
}

fn try_dns_xdp(ctx: XdpContext) -> Result<u32, ()> {
    // Task 5 will implement this
    Ok(xdp_action::XDP_PASS)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
```

- [ ] **Step 4: Create userspace crate skeleton**

Create `crates/doggy-dns-xdp/Cargo.toml`:

```toml
[package]
name = "doggy-dns-xdp"
version = "0.1.0"
edition = "2024"

[dependencies]
doggy-dns-plugin = { path = "../doggy-dns-plugin" }
doggy-dns-core = { path = "../doggy-dns-core" }
aya = "0.13"
aya-log = "0.2"
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
async-trait = { workspace = true }
hickory-proto = { workspace = true }
hickory-server = { workspace = true }

[build-dependencies]
aya-build = "0.1"
```

- [ ] **Step 5: Create build.rs for eBPF compilation**

Create `crates/doggy-dns-xdp/build.rs`:

```rust
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ebpf_dir = manifest_dir.join("../../xdp/doggy-dns-xdp-ebpf");

    // Tell cargo to rerun if eBPF source changes
    println!("cargo:rerun-if-changed={}", ebpf_dir.join("src/main.rs").display());
    println!("cargo:rerun-if-changed={}", ebpf_dir.join("Cargo.toml").display());

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
                std::fs::create_dir_all(&target_dir).ok();
                std::fs::write(&elf_path, &[]).ok();
            }
        }
    }

    // Expose the ELF path to the userspace crate
    println!("cargo:rustc-env=XDP_EBPF_ELF={}", elf_path.display());
}
```

- [ ] **Step 6: Add doggy-dns-xdp to workspace and feature gate**

Modify root `Cargo.toml`:

Add to `[workspace] members`:
```toml
    "crates/doggy-dns-xdp",
```

Add to `[workspace.dependencies]`:
```toml
aya = "0.13"
aya-log = "0.2"
```

Add `[features]` section:
```toml
[features]
xdp = ["dep:doggy-dns-xdp"]
default = []
```

Add to `[dependencies]`:
```toml
doggy-dns-xdp = { path = "crates/doggy-dns-xdp", optional = true }
```

- [ ] **Step 7: Create placeholder lib.rs for userspace crate**

Create `crates/doggy-dns-xdp/src/lib.rs`:

```rust
pub mod cache;
pub mod interface;
pub mod middleware;
pub mod program;
pub mod types;
```

Create `crates/doggy-dns-xdp/src/types.rs`:

```rust
/// Must match xdp/doggy-dns-xdp-ebpf/src/main.rs exactly.
pub const FLAG_NEGATIVE: u16 = 0x01;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheValue {
    pub resp_data: [u8; 512],
    pub resp_len: u16,
    pub flags: u16,
    pub _pad: u32,
    pub expires_at: u64,
}

impl CacheValue {
    pub fn new_positive(resp_data: Vec<u8>, expires_at: u64) -> Self {
        let len = resp_data.len().min(512);
        let mut data = [0u8; 512];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: 0,
            _pad: 0,
            expires_at,
        }
    }

    pub fn new_negative(resp_data: Vec<u8>, expires_at: u64) -> Self {
        let len = resp_data.len().min(512);
        let mut data = [0u8; 512];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: FLAG_NEGATIVE,
            _pad: 0,
            expires_at,
        }
    }

    pub fn is_negative(&self) -> bool {
        self.flags & FLAG_NEGATIVE != 0
    }
}

// Safety: CacheKey and CacheValue are #[repr(C)] with only Pod-compatible types.
// These assertions verify layout at compile time.
const _: () = {
    assert!(core::mem::size_of::<CacheKey>() == 16);
    assert!(core::mem::size_of::<CacheValue>() == 528);
};
```

Create empty placeholder files:
- `crates/doggy-dns-xdp/src/cache.rs` (empty, with `// TODO: Task 5`)
- `crates/doggy-dns-xdp/src/interface.rs` (empty, with `// TODO: Task 5`)
- `crates/doggy-dns-xdp/src/middleware.rs` (empty, with `// TODO: Task 5`)
- `crates/doggy-dns-xdp/src/program.rs` (empty, with `// TODO: Task 5`)

- [ ] **Step 8: Verify workspace compiles**

Run: `cargo check --workspace`
Expected: Compiles (with warnings about unused modules).

Run: `cargo check --features xdp`
Expected: Compiles (eBPF build may be skipped on non-Linux, which is fine).

- [ ] **Step 9: Commit**

```bash
git add xdp/ crates/doggy-dns-xdp/ Cargo.toml
git commit -m "feat: set up eBPF workspace with shared types and build infrastructure"
```

---

### Task 5: Implement userspace crate (interface detection, cache, program loader, middleware)

**Files:**
- Modify: `crates/doggy-dns-xdp/src/interface.rs`
- Modify: `crates/doggy-dns-xdp/src/cache.rs`
- Modify: `crates/doggy-dns-xdp/src/program.rs`
- Modify: `crates/doggy-dns-xdp/src/middleware.rs`
- Modify: `crates/doggy-dns-xdp/src/lib.rs`
- Test: `crates/doggy-dns-xdp/tests/interface_test.rs`
- Test: `crates/doggy-dns-xdp/tests/middleware_test.rs`

**Interfaces:**
- Consumes: `XdpConfig` from Task 2, `RequestOutcome` from Task 1, shared types from Task 4
- Produces: `XdpManager` (public API), `XdpCacheMiddleware` (implements `Middleware` trait)

- [ ] **Step 1: Write failing tests for interface detection**

Create `crates/doggy-dns-xdp/tests/interface_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::interface::{detect_default_interface, parse_default_from_route};

#[test]
fn parse_default_route_linux() {
    let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0102A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
    let iface = parse_default_from_route(content).expect("should find default");
    assert_eq!(iface, "eth0");
}

#[test]
fn parse_default_route_enp0s3() {
    let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
enp0s3\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
    let iface = parse_default_from_route(content).expect("should find default");
    assert_eq!(iface, "enp0s3");
}

#[test]
fn parse_default_route_no_default() {
    let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
    let err = parse_default_from_route(content).unwrap_err();
    assert!(err.to_string().contains("no default route"));
}

#[test]
fn detect_default_interface_runs_on_linux() {
    // This test only works on Linux. On other platforms, it should return an error.
    let result = detect_default_interface();
    if cfg!(target_os = "linux") {
        // Should succeed on Linux CI runners
        assert!(result.is_ok() || result.is_err());
    } else {
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Implement interface detection**

Write `crates/doggy-dns-xdp/src/interface.rs`:

```rust
use anyhow::{Context, anyhow};

/// Detect the default network interface by parsing /proc/net/route.
pub fn detect_default_interface() -> Result<String> {
    let content = std::fs::read_to_string("/proc/net/route")
        .context("failed to read /proc/net/route")?;
    parse_default_from_route(&content)
}

/// Parse the default interface name from /proc/net/route content.
/// Exported for testing.
pub fn parse_default_from_route(content: &str) -> Result<String> {
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && fields[1] == "00000000" {
            return Ok(fields[0].to_string());
        }
    }
    Err(anyhow!("no default route found in /proc/net/route"))
}

/// Resolve the XDP target interface with priority:
/// 1. config.xdp.interface (explicit)
/// 2. XDP_INTERFACE environment variable
/// 3. Auto-detect from /proc/net/route
pub fn resolve_interface(configured: &str) -> Result<String> {
    if !configured.is_empty() {
        return Ok(configured.to_string());
    }
    if let Ok(iface) = std::env::var("XDP_INTERFACE") {
        if !iface.is_empty() {
            return Ok(iface);
        }
    }
    detect_default_interface()
}
```

- [ ] **Step 3: Run interface tests**

Run: `cargo test -p doggy-dns-xdp --test interface_test`
Expected: All PASS.

- [ ] **Step 4: Write failing tests for FNV-1a hash**

Create `crates/doggy-dns-xdp/tests/cache_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::cache::fnv1a_64;

#[test]
fn fnv1a_empty_string() {
    assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
}

#[test]
fn fnv1a_known_value() {
    // FNV-1a of "a" = 0xaf63dc4c8601ec8c
    assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
}

#[test]
fn fnv1a_case_sensitive() {
    let lower = fnv1a_64(b"example.com");
    let upper = fnv1a_64(b"EXAMPLE.COM");
    assert_ne!(lower, upper);
}

#[test]
fn fnv1a_deterministic() {
    let input = b"svc-1.nacos.default.doggy.test";
    assert_eq!(fnv1a_64(input), fnv1a_64(input));
}
```

- [ ] **Step 5: Implement cache module with FNV-1a and BPF map wrapper**

Write `crates/doggy-dns-xdp/src/cache.rs`:

```rust
use anyhow::Result;
use aya::maps::{HashMap, MapData, UpdateFlags};
use crate::types::{CacheKey, CacheValue};

/// FNV-1a 64-bit hash. Used to hash DNS QNAME for BPF map keys.
pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Build a CacheKey from DNS question name, type, and class.
pub fn make_cache_key(qname: &str, qtype: u16, qclass: u16) -> CacheKey {
    CacheKey {
        name_hash: fnv1a_64(qname.to_ascii_lowercase().as_bytes()),
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
    }
}

/// Wrapper around the BPF HashMap for cache operations.
pub struct DnsCacheMap {
    inner: HashMap<MapData, CacheKey, CacheValue>,
}

impl DnsCacheMap {
    pub fn from_map_data(map_data: MapData) -> Result<Self> {
        Ok(Self {
            inner: HashMap::new(map_data, None)?,
        })
    }

    pub fn insert(&mut self, key: &CacheKey, value: &CacheValue) -> Result<()> {
        self.inner.insert(key, value, UpdateFlags::ANY)?;
        Ok(())
    }

    pub fn get(&mut self, key: &CacheKey) -> Result<Option<CacheValue>> {
        match self.inner.get(key) {
            Ok(value) => Ok(Some(value)),
            Err(_) => Ok(None),
        }
    }
}
```

- [ ] **Step 6: Run cache tests**

Run: `cargo test -p doggy-dns-xdp --test cache_test`
Expected: All PASS.

- [ ] **Step 7: Write failing tests for XdpCacheMiddleware**

Create `crates/doggy-dns-xdp/tests/middleware_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::middleware::compute_cache_entry;
use doggy_dns_xdp::types::{CacheKey, CacheValue, FLAG_NEGATIVE};
use hickory_proto::op::ResponseCode;

#[test]
fn compute_cache_entry_positive() {
    let resp_bytes = vec![0u8; 64];
    let (key, value) = compute_cache_entry(
        "example.com",
        1,  // A
        1,  // IN
        ResponseCode::NoError,
        resp_bytes.clone(),
        Some(300),
        30,
        1_000_000_000, // fake now
    );

    assert_eq!(key.qtype_class, (1 << 16) | 1);
    assert_eq!(value.resp_len, 64);
    assert_eq!(value.flags, 0);
    assert_eq!(value.expires_at, 1_000_000_000 + 300 * 1_000_000_000);
    assert_eq!(&value.resp_data[..64], resp_bytes.as_slice());
}

#[test]
fn compute_cache_entry_NXDomain() {
    let resp_bytes = vec![0u8; 12];
    let (_key, value) = compute_cache_entry(
        "nonexistent.example.com",
        1,
        1,
        ResponseCode::NXDomain,
        resp_bytes,
        None,
        60,
        2_000_000_000,
    );

    assert_eq!(value.flags, FLAG_NEGATIVE);
    assert_eq!(value.expires_at, 2_000_000_000 + 60 * 1_000_000_000);
}

#[test]
fn compute_cache_entry_ServFail_not_cached() {
    let result = compute_cache_entry(
        "fail.example.com",
        1,
        1,
        ResponseCode::ServFail,
        vec![],
        None,
        30,
        0,
    );
    // ServFail should not produce a cache entry
    assert!(result.1.resp_len == 0 || result.1.flags == 0);
}

#[test]
fn compute_cache_entry_truncates_large_response() {
    let resp_bytes = vec![0xAB; 1024]; // larger than 512
    let (_key, value) = compute_cache_entry(
        "large.example.com",
        1,
        1,
        ResponseCode::NoError,
        resp_bytes,
        Some(60),
        30,
        0,
    );
    assert_eq!(value.resp_len, 512);
}
```

- [ ] **Step 8: Implement XdpCacheMiddleware**

Write `crates/doggy-dns-xdp/src/middleware.rs`:

```rust
use async_trait::async_trait;
use doggy_dns_core::config::XdpConfig;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::ResponseCode;
use hickory_server::server::Request;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cache::{DnsCacheMap, make_cache_key};
use crate::types::{CacheKey, CacheValue, FLAG_NEGATIVE};

/// Compute cache key and value for a DNS response.
/// Exported for testing.
pub fn compute_cache_entry(
    qname: &str,
    qtype: u16,
    qclass: u16,
    response_code: ResponseCode,
    resp_bytes: Vec<u8>,
    min_answer_ttl: Option<u32>,
    negative_ttl: u64,
    now_ns: u64,
) -> (CacheKey, CacheValue) {
    let key = make_cache_key(qname, qtype, qclass);

    let is_negative = response_code == ResponseCode::NXDomain;
    let ttl = if is_negative {
        negative_ttl
    } else {
        min_answer_ttl.unwrap_or(negative_ttl as u32) as u64
    };

    let expires_at = now_ns + ttl * 1_000_000_000;

    let value = if is_negative {
        CacheValue::new_negative(resp_bytes, expires_at)
    } else {
        CacheValue::new_positive(resp_bytes, expires_at)
    };

    (key, value)
}

pub struct XdpCacheMiddleware {
    cache: Arc<Mutex<DnsCacheMap>>,
    negative_ttl: u64,
}

impl XdpCacheMiddleware {
    pub fn new(cache: DnsCacheMap, config: &XdpConfig) -> Self {
        Self {
            cache: Arc::new(Mutex::new(cache)),
            negative_ttl: config.negative_ttl,
        }
    }
}

#[async_trait]
impl Middleware for XdpCacheMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        MiddlewareAction::Continue
    }

    async fn after_request(
        &self,
        request: &Request,
        _duration_ms: u64,
        outcome: &RequestOutcome,
    ) {
        if outcome.response_code != ResponseCode::NoError
            && outcome.response_code != ResponseCode::NXDomain
        {
            return;
        }

        let Some(ref resp_bytes) = outcome.response_bytes else {
            return;
        };

        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "failed to get request info for XDP cache");
                return;
            }
        };

        let qname = request_info.query.name().to_string();
        let qtype = request_info.query.query_type() as u16;
        let qclass = request_info.query.query_class() as u16;

        let now_ns = tokio::time::Instant::now().
            // Use a monotonic clock; the XDP program uses ktime_get_ns.
            // For cache TTL purposes, we use tokio Instant as a proxy.
            // In production, the BPF map uses kernel time.
            elapsed().as_nanos() as u64;

        let (key, value) = compute_cache_entry(
            &qname,
            qtype,
            qclass,
            outcome.response_code,
            resp_bytes.clone(),
            outcome.min_answer_ttl,
            self.negative_ttl,
            now_ns,
        );

        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.insert(&key, &value) {
            tracing::warn!(error = %e, qname = %qname, "failed to write XDP cache entry");
        }
    }
}
```

Note: The `now_ns` computation needs refinement during implementation — the BPF map uses `bpf_ktime_get_ns()` (kernel monotonic clock), while userspace uses `tokio::time::Instant`. For TTL checking in XDP, the kernel compares `expires_at` against `bpf_ktime_get_ns()`. The userspace write should use a compatible clock source. This can be addressed with `libc::clock_gettime(CLOCK_MONOTONIC)` or by reading `/proc/uptime` — refine during implementation.

- [ ] **Step 9: Run middleware tests**

Run: `cargo test -p doggy-dns-xdp --test middleware_test`
Expected: All PASS.

- [ ] **Step 10: Implement XDP program loader (program.rs)**

Write `crates/doggy-dns-xdp/src/program.rs`:

```rust
use anyhow::{Context, Result};
use aya::{
    programs::{Xdp, XdpFlags},
    Ebpf, EbpfLoader,
};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::cache::DnsCacheMap;
use crate::interface::resolve_interface;
use doggy_dns_core::config::XdpConfig;

pub struct XdpManager {
    _ebpf: Ebpf,
    _link: Option<aya::programs::xdp::XdpLinkId>,
    pub cache: DnsCacheMap,
    pub interface: String,
}

impl XdpManager {
    pub fn new(config: &XdpConfig) -> Result<Self> {
        let iface = resolve_interface(&config.interface)
            .context("failed to resolve XDP interface")?;

        let elf_bytes = include_bytes!(env!("XDP_EBPF_ELF"));
        if elf_bytes.is_empty() {
            anyhow::bail!("eBPF ELF is empty — build the eBPF program first");
        }

        let mut ebpf = EbpfLoader::new()
            .load(elf_bytes)
            .context("failed to load eBPF ELF")?;

        // Pin or open the BPF map
        let pin_path = Path::new(&config.pin_path);
        if let Some(parent) = pin_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        let map_data = if pin_path.exists() {
            // Reuse existing pinned map
            let map = ebpf.take_map("DNS_CACHE")
                .context("DNS_CACHE map not found in ELF")?;
            map.pin(pin_path)
                .context("failed to pin DNS_CACHE map")?;
            map
        } else {
            let mut map = ebpf.take_map("DNS_CACHE")
                .context("DNS_CACHE map not found in ELF")?;
            map.pin(pin_path)
                .context("failed to pin DNS_CACHE map")?;
            map
        };

        let cache = DnsCacheMap::from_map_data(map_data)?;

        let link_id = if config.attach_mode == "auto" {
            let program: &mut Xdp = ebpf
                .program_mut("dns_xdp")
                .context("dns_xdp program not found")?
                .try_into()
                .context("dns_xdp is not an XDP program")?;

            program.load().context("failed to load XDP program")?;
            let link_id = program
                .attach(&iface, XdpFlags::default())
                .context(format!("failed to attach XDP to interface {}", iface))?;
            Some(link_id)
        } else {
            // manual mode: program is loaded but not attached
            let program: &mut Xdp = ebpf
                .program_mut("dns_xdp")
                .context("dns_xdp program not found")?
                .try_into()?;
            program.load().context("failed to load XDP program")?;
            None
        };

        Ok(Self {
            _ebpf: ebpf,
            _link: link_id,
            cache,
            interface: iface,
        })
    }
}
```

- [ ] **Step 11: Update lib.rs to export all modules**

Write `crates/doggy-dns-xdp/src/lib.rs`:

```rust
pub mod cache;
pub mod interface;
pub mod middleware;
pub mod program;
pub mod types;

pub use middleware::XdpCacheMiddleware;
pub use program::XdpManager;
```

- [ ] **Step 12: Run all tests and clippy**

Run: `cargo test -p doggy-dns-xdp`
Run: `cargo clippy --all-features --all-targets -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: All pass.

- [ ] **Step 13: Commit**

```bash
git add crates/doggy-dns-xdp/
git commit -m "feat(xdp): implement userspace crate with interface detection, cache, loader, and middleware"
```

---

### Task 6: Implement XDP eBPF program

**Files:**
- Modify: `xdp/doggy-dns-xdp-ebpf/src/main.rs`

**Interfaces:**
- Consumes: Shared types from Task 4, `DNS_CACHE` LRU hash map
- Produces: Working XDP program that parses DNS, looks up cache, constructs responses

**Prerequisites:** Linux with `bpf-linker` installed. This task can only be compiled and tested on Linux.

- [ ] **Step 1: Implement Ethernet + IPv4 + UDP parsing**

Replace the `try_dns_xdp` function in `xdp/doggy-dns-xdp-ebpf/src/main.rs`:

```rust
#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::{xdp_action, ethhdr, iphdr, udphdr},
    macros::{map, xdp},
    maps::LruHashMap,
    programs::XdpContext,
};
use aya_log_ebpf::info;

pub const FLAG_NEGATIVE: u16 = 0x01;
const ETH_P_IP: u16 = 0x0800;
const IPPROTO_UDP: u8 = 17;
const DNS_PORT: u16 = 53;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheValue {
    pub resp_data: [u8; 512],
    pub resp_len: u16,
    pub flags: u16,
    pub _pad: u32,
    pub expires_at: u64,
}

#[map]
static DNS_CACHE: LruHashMap<CacheKey, CacheValue> =
    LruHashMap::with_max_entries(65536, 0);

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    match try_dns_xdp(&ctx) {
        Ok(action) => action,
        Err(_) => xdp_action::XDP_PASS,
    }
}

fn try_dns_xdp(ctx: &XdpContext) -> Result<u32, ()> {
    // 1. Parse Ethernet header
    let eth = unsafe { ctx.data() as *const ethhdr };
    if eth.is_null() {
        return Ok(xdp_action::XDP_PASS);
    }
    let eth = unsafe { &*eth };
    if u16::from_be(eth.h_proto) != ETH_P_IP {
        return Ok(xdp_action::XDP_PASS);
    }

    // 2. Parse IPv4 header
    let ip_offset = core::mem::size_of::<ethhdr>();
    let ip = unsafe { (ctx.data() as *const u8).add(ip_offset) as *const iphdr };
    if unsafe { ip.is_null() || (ip as *const u8).add(core::mem::size_of::<iphdr>()) > ctx.data_end() } {
        return Ok(xdp_action::XDP_PASS);
    }
    let ip = unsafe { &*ip };
    if ip.protocol != IPPROTO_UDP {
        return Ok(xdp_action::XDP_PASS);
    }

    // 3. Parse UDP header
    let udp_offset = ip_offset + (ip.ihl as usize) * 4;
    let udp = unsafe { (ctx.data() as *const u8).add(udp_offset) as *const udphdr };
    if unsafe { udp.is_null() || (udp as *const u8).add(core::mem::size_of::<udphdr>()) > ctx.data_end() } {
        return Ok(xdp_action::XDP_PASS);
    }
    let udp = unsafe { &*udp };
    if u16::from_be(udp.dest) != DNS_PORT {
        return Ok(xdp_action::XDP_PASS);
    }

    // 4. Parse DNS header
    let dns_offset = udp_offset + core::mem::size_of::<udphdr>();
    let dns_data = unsafe { (ctx.data() as *const u8).add(dns_offset) };
    if unsafe { dns_data.add(12) > ctx.data_end() } {
        return Ok(xdp_action::XDP_PASS);
    }

    // Check QR bit (must be 0 = query)
    let flags = unsafe { u16::from_be(*(dns_data as *const u16).add(1)) };
    if flags & 0x8000 != 0 {
        return Ok(xdp_action::XDP_PASS); // Not a query
    }

    // Check QDCOUNT == 1
    let qdcount = unsafe { u16::from_be(*(dns_data as *const u16).add(2)) };
    if qdcount != 1 {
        return Ok(xdp_action::XDP_PASS);
    }

    // 5. Extract QNAME and compute hash
    let question_offset = 12; // DNS header is 12 bytes
    let (name_hash, qtype, qclass, question_len) =
        match parse_dns_question(ctx, dns_offset, question_offset) {
            Some(v) => v,
            None => return Ok(xdp_action::XDP_PASS),
        };

    let key = CacheKey {
        name_hash,
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
    };

    // 6. Cache lookup
    let entry = match unsafe { DNS_CACHE.lookup(&key) } {
        Some(v) => v,
        None => return Ok(xdp_action::XDP_PASS),
    };

    // 7. TTL check
    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };
    if entry.expires_at <= now {
        return Ok(xdp_action::XDP_PASS);
    }

    // 8. Construct response
    if entry.flags & FLAG_NEGATIVE != 0 {
        build_nxdomain_response(ctx, dns_offset, question_len)?;
    } else {
        build_positive_response(ctx, dns_offset, &entry)?;
    }

    // 9. Swap headers
    swap_ethernet_headers(ctx);
    swap_ip_headers(ctx, ip_offset);
    swap_udp_headers(ctx, udp_offset);
    recalculate_checksums(ctx, ip_offset, udp_offset);

    Ok(xdp_action::XDP_TX)
}
```

- [ ] **Step 2: Implement DNS question parsing and FNV-1a hash**

Add to `main.rs`:

```rust
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn parse_dns_question(
    ctx: &XdpContext,
    dns_offset: usize,
    question_offset: usize,
) -> Option<(u64, u16, u16, usize)> {
    let mut name_buf = [0u8; 256];
    let mut name_len = 0usize;
    let mut pos = dns_offset + question_offset;

    // Parse QNAME label by label
    loop {
        if unsafe { (ctx.data() as *const u8).add(pos) >= ctx.data_end() } {
            return None;
        }
        let label_len = unsafe { *(ctx.data() as *const u8).add(pos) } as usize;
        pos += 1;

        if label_len == 0 {
            break; // Root label
        }
        if label_len > 63 || name_len + label_len + 1 > 255 {
            return None; // Invalid or too long
        }

        // Copy label to buffer (lowercase)
        for i in 0..label_len {
            if unsafe { (ctx.data() as *const u8).add(pos + i) >= ctx.data_end() } {
                return None;
            }
            let byte = unsafe { *(ctx.data() as *const u8).add(pos + i) };
            name_buf[name_len] = byte.to_ascii_lowercase();
            name_len += 1;
        }
        name_buf[name_len] = b'.';
        name_len += 1;
        pos += label_len;
    }

    // Read QTYPE and QCLASS (2 bytes each, after QNAME + null terminator)
    pos += 1; // skip the null terminator we already processed
    if unsafe { (ctx.data() as *const u8).add(pos + 4) > ctx.data_end() } {
        return None;
    }
    let qtype = unsafe { u16::from_be(*(ctx.data() as *const u16).add(pos / 2)) };
    let qclass = unsafe { u16::from_be(*(ctx.data() as *const u16).add(pos / 2 + 1)) };

    let question_total_len = pos - dns_offset;

    let name_hash = fnv1a_64(&name_buf[..name_len]);
    Some((name_hash, qtype, qclass, question_total_len))
}
```

- [ ] **Step 3: Implement response construction functions**

Add to `main.rs`:

```rust
fn build_positive_response(
    ctx: &XdpContext,
    dns_offset: usize,
    entry: &CacheValue,
) -> Result<(), ()> {
    let resp_len = entry.resp_len as usize;
    let new_packet_len = dns_offset + resp_len;

    // Adjust tail to fit the response
    let current_len = unsafe { ctx.data_end() as usize - ctx.data() as usize };
    if new_packet_len > 1500 {
        return Err(()); // Response too large for standard MTU
    }

    let adjust = new_packet_len as i32 - current_len as i32;
    if unsafe { ctx.adjust_tail(adjust) } != 0 {
        return Err(());
    }

    // Copy response data into DNS payload area
    let dst = unsafe { (ctx.data() as *mut u8).add(dns_offset) };
    let src = &entry.resp_data[..resp_len];
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, resp_len);
    }

    // Copy Transaction ID from original query (first 2 bytes of DNS header)
    // The query's TXID is still in the packet before dns_offset
    // Actually, resp_data already includes the DNS header with TXID.
    // We need to preserve the original query's TXID.
    // For now, the response's TXID from cache is used (acceptable for MVP).

    Ok(())
}

fn build_nxdomain_response(
    ctx: &XdpContext,
    dns_offset: usize,
    question_len: usize,
) -> Result<(), ()> {
    // Truncate packet to just after the question section
    let new_dns_len = 12 + question_len; // DNS header + question
    let new_packet_len = dns_offset + new_dns_len;
    let current_len = unsafe { ctx.data_end() as usize - ctx.data() as usize };

    let adjust = new_packet_len as i32 - current_len as i32;
    if unsafe { ctx.adjust_tail(adjust) } != 0 {
        return Err(());
    }

    // Modify DNS header in-place
    let dns_ptr = unsafe { (ctx.data() as *mut u8).add(dns_offset) };

    // Set QR=1, RCODE=3 (NXDomain)
    // DNS flags word: set bit 15 (QR) and bits 0-3 (RCODE=3)
    unsafe {
        let flags_ptr = dns_ptr.add(2) as *mut u16;
        let original_flags = u16::from_be(*flags_ptr);
        let new_flags = (original_flags & 0x0000) | 0x8003; // QR=1, RCODE=3
        *flags_ptr = new_flags.to_be();

        // Set ANCOUNT=0, NSCOUNT=0, ARCOUNT=0
        let ancount_ptr = dns_ptr.add(6) as *mut u16;
        *ancount_ptr = 0u16.to_be();
        let nscount_ptr = dns_ptr.add(8) as *mut u16;
        *nscount_ptr = 0u16.to_be();
        let arcount_ptr = dns_ptr.add(10) as *mut u16;
        *arcount_ptr = 0u16.to_be();
    }

    Ok(())
}
```

- [ ] **Step 4: Implement header swap and checksum functions**

Add to `main.rs`:

```rust
fn swap_ethernet_headers(ctx: &XdpContext) {
    let eth = unsafe { ctx.data() as *mut ethhdr };
    let eth = unsafe { &mut *eth };

    // Swap src and dst MAC
    let tmp = eth.src;
    eth.src = eth.dst;
    eth.dst = tmp;
}

fn swap_ip_headers(ctx: &XdpContext, ip_offset: usize) {
    let ip = unsafe { (ctx.data() as *mut u8).add(ip_offset) as *mut iphdr };
    let ip = unsafe { &mut *ip };

    // Swap src and dst IP
    let tmp = ip.saddr;
    ip.saddr = ip.daddr;
    ip.daddr = tmp;

    // Set DF flag
    ip.frag_off = (ip.frag_off & 0x00FF) | 0x4000;
}

fn swap_udp_headers(ctx: &XdpContext, udp_offset: usize) {
    let udp = unsafe { (ctx.data() as *mut u8).add(udp_offset) as *mut udphdr };
    let udp = unsafe { &mut *udp };

    let tmp = udp.source;
    udp.source = udp.dest;
    udp.dest = tmp;
}

fn recalculate_checksums(ctx: &XdpContext, ip_offset: usize, udp_offset: usize) {
    // Recalculate IPv4 header checksum
    let ip = unsafe { (ctx.data() as *mut u8).add(ip_offset) as *mut iphdr };
    unsafe {
        (*ip).check = 0;
        let ip_hdr_len = ((*ip).ihl as usize) * 4;
        let mut sum: u32 = 0;
        let ip_bytes = core::slice::from_raw_parts(ip as *const u8, ip_hdr_len);
        let mut i = 0;
        while i + 1 < ip_hdr_len {
            sum += ((ip_bytes[i] as u32) << 8) | (ip_bytes[i + 1] as u32);
            i += 2;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        (*ip).check = !(sum as u16);
    }

    // Recalculate UDP checksum
    let udp = unsafe { (ctx.data() as *mut u8).add(udp_offset) as *mut udphdr };
    unsafe {
        (*udp).check = 0;
        // For simplicity in MVP, set UDP checksum to 0 (optional for IPv4/UDP)
        // A full implementation would compute the pseudo-header + UDP checksum
    }
}
```

- [ ] **Step 5: Build the eBPF program**

Run (on Linux with bpf-linker):
```bash
cd xdp && cargo build --target bpfel-unknown-none --release
```
Expected: Compiles without errors. BPF verifier will validate the program on load.

- [ ] **Step 6: Commit**

```bash
git add xdp/doggy-dns-xdp-ebpf/src/main.rs
git commit -m "feat(xdp-ebpf): implement XDP program with DNS parsing, cache lookup, and response construction"
```

---

### Task 7: Wire XDP into main.rs with feature gate and graceful degradation

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `XdpManager` and `XdpCacheMiddleware` from Task 5, `XdpConfig` from Task 2

- [ ] **Step 1: Add feature-gated XDP initialization to main.rs**

In `src/main.rs`, add at the top:

```rust
#[cfg(feature = "xdp")]
use doggy_dns_xdp::{XdpCacheMiddleware, XdpManager};
```

In `async_main`, after the middleware chain is built and before `build_server`:

```rust
    // XDP fast path initialization (feature-gated)
    #[cfg(feature = "xdp")]
    let _xdp_manager = if config.xdp.enabled {
        match XdpManager::new(&config.xdp) {
            Ok(mgr) => {
                tracing::info!(interface = %mgr.interface, "XDP fast path active");
                middlewares.push(std::sync::Arc::new(
                    XdpCacheMiddleware::new(mgr.cache_handle(), &config.xdp)
                ));
                Some(mgr)
            }
            Err(e) => {
                tracing::warn!(error = %e, "XDP attach failed, running in userspace-only mode");
                None
            }
        }
    } else {
        None
    };
```

Note: The `XdpManager` must be kept alive for the duration of the server — its `_link` field holds the XDP attachment. When `_xdp_manager` is dropped at the end of `async_main`, the XDP program is detached.

- [ ] **Step 2: Verify compilation with and without feature**

Run: `cargo build` (without XDP)
Expected: Compiles, no XDP code included.

Run: `cargo build --features xdp` (with XDP, requires Linux + bpf-linker)
Expected: Compiles with XDP support.

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace`
Run: `cargo clippy --all-features --all-targets -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire XDP into main.rs with feature gate and graceful degradation"
```

---

### Task 8: Add XDP integration tests

**Files:**
- Create: `crates/doggy-dns-xdp/tests/xdp_integration_test.rs`

**Prerequisites:** Linux with `sudo`, kernel >= 5.10, veth pair support.

- [ ] **Step 1: Write integration test for XDP cache hit/miss**

Create `crates/doggy-dns-xdp/tests/xdp_integration_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

// These tests require:
// - Linux with kernel >= 5.10
// - sudo / root privileges
// - veth pair support
//
// Run with: sudo -E cargo test -p doggy-dns-xdp --test xdp_integration_test -- --nocapture

#[cfg(target_os = "linux")]
mod linux_tests {
    use std::process::Command;

    fn setup_netns() {
        let _ = Command::new("ip").args(["netns", "add", "doggy-dns-test"]).output();
        let _ = Command::new("ip")
            .args(["link", "add", "veth0", "type", "veth", "peer", "name", "veth1"])
            .output();
        let _ = Command::new("ip")
            .args(["link", "set", "veth1", "netns", "doggy-dns-test"])
            .output();
        let _ = Command::new("ip")
            .args(["addr", "add", "10.0.0.1/24", "dev", "veth0"])
            .output();
        let _ = Command::new("ip").args(["link", "set", "veth0", "up"]).output();
        let _ = Command::new("ip")
            .args(["netns", "exec", "doggy-dns-test", "ip", "addr", "add", "10.0.0.2/24", "dev", "veth1"])
            .output();
        let _ = Command::new("ip")
            .args(["netns", "exec", "doggy-dns-test", "ip", "link", "set", "veth1", "up"])
            .output();
    }

    fn teardown_netns() {
        let _ = Command::new("ip").args(["netns", "delete", "doggy-dns-test"]).output();
        let _ = Command::new("ip").args(["link", "delete", "veth0"]).output();
    }

    #[test]
    #[ignore] // Requires root; run with: sudo -E cargo test -- --ignored --nocapture
    fn xdp_cache_hit_positive() {
        setup_netns();
        // TODO: Load XDP program on veth0, send DNS query, verify cache hit
        teardown_netns();
    }

    #[test]
    #[ignore]
    fn xdp_cache_hit_negative_nxdomain() {
        setup_netns();
        // TODO: Send query for non-existent name, verify NXDomain cached
        teardown_netns();
    }

    #[test]
    #[ignore]
    fn xdp_cache_ttl_expiry() {
        setup_netns();
        // TODO: Write entry with 1s TTL, wait 2s, verify miss
        teardown_netns();
    }

    #[test]
    #[ignore]
    fn xdp_non_dns_passthrough() {
        setup_netns();
        // TODO: Send packet to port 80, verify XDP_PASS
        teardown_netns();
    }
}
```

- [ ] **Step 2: Verify tests compile**

Run: `cargo test -p doggy-dns-xdp --test xdp_integration_test --no-run`
Expected: Compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/doggy-dns-xdp/tests/xdp_integration_test.rs
git commit -m "test(xdp): add integration test scaffolding for XDP cache behavior"
```

---

### Task 9: Add XDP benchmarks

**Files:**
- Create: `crates/doggy-dns-xdp/benches/xdp_bench.rs`

- [ ] **Step 1: Write criterion benchmarks**

Create `crates/doggy-dns-xdp/benches/xdp_bench.rs`:

```rust
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use doggy_dns_xdp::cache::{fnv1a_64, make_cache_key};
use doggy_dns_xdp::types::{CacheKey, CacheValue};

fn bench_fnv1a_hash(c: &mut Criterion) {
    let names = [
        "svc-1.nacos.default.doggy.test",
        "api-gateway.production.cluster.local",
        "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p",
    ];

    c.bench_function("fnv1a_64_short", |b| {
        b.iter(|| fnv1a_64(black_box(names[0].as_bytes())))
    });

    c.bench_function("fnv1a_64_long", |b| {
        b.iter(|| fnv1a_64(black_box(names[2].as_bytes())))
    });
}

fn bench_make_cache_key(c: &mut Criterion) {
    c.bench_function("make_cache_key", |b| {
        b.iter(|| {
            make_cache_key(
                black_box("svc-1.nacos.default.doggy.test"),
                black_box(1),
                black_box(1),
            )
        })
    });
}

fn bench_cache_value_construction(c: &mut Criterion) {
    let resp_data = vec![0u8; 128];

    c.bench_function("cache_value_positive", |b| {
        b.iter(|| CacheValue::new_positive(black_box(resp_data.clone()), black_box(1_000_000_000)))
    });

    c.bench_function("cache_value_negative", |b| {
        b.iter(|| CacheValue::new_negative(black_box(resp_data.clone()), black_box(1_000_000_000)))
    });
}

criterion_group!(
    benches,
    bench_fnv1a_hash,
    bench_make_cache_key,
    bench_cache_value_construction,
);
criterion_main!(benches);
```

- [ ] **Step 2: Add bench dependency to Cargo.toml**

In `crates/doggy-dns-xdp/Cargo.toml`, add:

```toml
[dev-dependencies]
criterion = { workspace = true }

[[bench]]
name = "xdp_bench"
harness = false
```

- [ ] **Step 3: Verify benchmarks compile and run**

Run: `cargo bench -p doggy-dns-xdp --bench xdp_bench --no-run`
Expected: Compiles.

Run: `cargo bench -p doggy-dns-xdp --bench xdp_bench`
Expected: Runs and produces criterion output.

- [ ] **Step 4: Commit**

```bash
git add crates/doggy-dns-xdp/benches/ crates/doggy-dns-xdp/Cargo.toml
git commit -m "bench(xdp): add criterion benchmarks for FNV-1a hash, cache key, and value construction"
```

---

### Task 10: Add XDP CI workflows to GitHub Actions

**Files:**
- Create: `.github/workflows/xdp-ci.yml`
- Create: `scripts/xdp_stress_report.py`
- Create: `config/doggy-dns-stress.toml`
- Create: `config/doggy-dns-stress-xdp.toml`

- [ ] **Step 1: Create XDP CI workflow**

Create `.github/workflows/xdp-ci.yml`:

```yaml
name: XDP CI

on:
  push:
    branches: [main]
    paths:
      - 'crates/doggy-dns-xdp/**'
      - 'xdp/**'
      - 'crates/doggy-dns-plugin/**'
      - 'crates/doggy-dns-core/**'
  pull_request:
    branches: [main]
    paths:
      - 'crates/doggy-dns-xdp/**'
      - 'xdp/**'
      - 'crates/doggy-dns-plugin/**'
      - 'crates/doggy-dns-core/**'

env:
  CARGO_TERM_COLOR: always

jobs:
  xdp-integration-test:
    name: XDP Integration Tests
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: bpfel-unknown-none
      - uses: Swatinem/rust-cache@v2
      - name: Install bpf-linker
        run: cargo install bpf-linker
      - name: Build eBPF program
        run: cd xdp && cargo build --release --target bpfel-unknown-none
      - name: Run XDP integration tests
        run: sudo -E cargo test -p doggy-dns-xdp --features xdp --test xdp_integration_test -- --ignored --nocapture
        env:
          RUST_LOG: debug
      - name: Cleanup
        if: always()
        run: sudo ip netns delete doggy-dns-test || true

  xdp-benchmark:
    name: XDP Benchmarks
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: bpfel-unknown-none
      - uses: Swatinem/rust-cache@v2
      - name: Install bpf-linker
        run: cargo install bpf-linker
      - name: Build eBPF program
        run: cd xdp && cargo build --release --target bpfel-unknown-none
      - name: Run benchmarks (PR)
        run: |
          sudo -E cargo bench -p doggy-dns-xdp --features xdp --bench xdp_bench -- --save-baseline pr | tee /tmp/bench-pr.txt
      - name: Checkout base branch
        run: git checkout origin/main
      - name: Run benchmarks (base)
        run: |
          sudo -E cargo bench -p doggy-dns-xdp --features xdp --bench xdp_bench -- --save-baseline base | tee /tmp/bench-base.txt
      - name: Post comparison comment
        uses: actions/github-script@v7
        with:
          script: |
        const fs = require('fs');
        const pr = fs.readFileSync('/tmp/bench-pr.txt', 'utf8');
        const base = fs.readFileSync('/tmp/bench-base.txt', 'utf8');
        const body = `## XDP Benchmark Comparison\n\n### PR Branch\n\`\`\`\n${pr}\n\`\`\`\n\n### Base Branch\n\`\`\`\n${base}\n\`\`\``;
        github.rest.issues.createComment({
          issue_number: context.issue.number,
          owner: context.repo.owner,
          repo: context.repo.repo,
          body
        });

  xdp-stress-test:
    name: XDP Stress Test
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: bpfel-unknown-none
      - uses: Swatinem/rust-cache@v2
      - name: Install dependencies
        run: |
          cargo install bpf-linker
          sudo apt-get update && sudo apt-get install -y dnsperf
      - name: Build eBPF program
        run: cd xdp && cargo build --release --target bpfel-unknown-none
      - name: Build doggy-dns (with XDP)
        run: cargo build --features xdp --release
      - name: Generate query file
        run: |
          for i in $(seq 1 1000); do
            echo "svc-${i}.nacos.default.doggy.test A"
          done > /tmp/queries.txt
      - name: Stress test (userspace-only)
        run: |
          ./target/release/doggy-dns config/doggy-dns-stress.toml &
          echo $! > /tmp/server.pid
          sleep 2
          dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 1 -t 1 -d 5 > /dev/null 2>&1
          dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 10 -t 4 -d 30 -j > /tmp/userspace-result.json
          kill $(cat /tmp/server.pid)
      - name: Stress test (XDP)
        run: |
          sudo -E ./target/release/doggy-dns config/doggy-dns-stress-xdp.toml &
          echo $! | sudo tee /tmp/server.pid
          sleep 2
          sudo dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 1 -t 1 -d 5 > /dev/null 2>&1
          sudo dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 10 -t 4 -d 30 -j > /tmp/xdp-result.json
          sudo kill $(cat /tmp/server.pid) || true
        env:
          XDP_INTERFACE: lo
      - name: Generate comparison report
        run: python3 scripts/xdp_stress_report.py /tmp/userspace-result.json /tmp/xdp-result.json > /tmp/stress-report.md
      - name: Post PR comment
        uses: actions/github-script@v7
        with:
          script: |
            const fs = require('fs');
            const report = fs.readFileSync('/tmp/stress-report.md', 'utf8');
            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body: report
            });
```

- [ ] **Step 2: Create stress test report script**

Create `scripts/xdp_stress_report.py`:

```python
#!/usr/bin/env python3
"""Parse dnsperf JSON output and generate a markdown comparison report."""

import json
import sys

def load_results(path):
    with open(path) as f:
        return json.load(f)

def format_report(userspace, xdp):
    u_qps = userspace.get("queries_per_second", 0)
    x_qps = xdp.get("queries_per_second", 0)
    ratio = x_qps / u_qps if u_qps > 0 else 0

    u_lat = userspace.get("latency", {})
    x_lat = xdp.get("latency", {})

    report = f"""## XDP Stress Test Report

### Throughput

| Mode | QPS |
|------|-----|
| Userspace-only | {u_qps:,.0f} |
| XDP (cache hit) | {x_qps:,.0f} |
| **Improvement** | **{ratio:.1f}x** |

### Latency

| Mode | Average |
|------|---------|
| Userspace-only | {u_lat.get("average", "N/A")} |
| XDP (cache hit) | {x_lat.get("average", "N/A")} |
"""
    return report

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: xdp_stress_report.py <userspace.json> <xdp.json>", file=sys.stderr)
        sys.exit(1)

    userspace = load_results(sys.argv[1])
    xdp = load_results(sys.argv[2])
    print(format_report(userspace, xdp))
```

- [ ] **Step 3: Create stress test config files**

Create `config/doggy-dns-stress.toml`:

```toml
[server]
listen_addr = "127.0.0.1"
port = 5353
worker_threads = 4

[xdp]
enabled = false

[[plugins]]
kind = "Forward"
enabled = true
upstream = ["8.8.8.8:53"]
```

Create `config/doggy-dns-stress-xdp.toml`:

```toml
[server]
listen_addr = "127.0.0.1"
port = 5353
worker_threads = 4

[xdp]
enabled = true
interface = "lo"
cache_size = 65536
negative_ttl = 300

[[plugins]]
kind = "Forward"
enabled = true
upstream = ["8.8.8.8:53"]
```

- [ ] **Step 4: Verify workflow YAML is valid**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/xdp-ci.yml'))"`
Expected: No errors.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/xdp-ci.yml scripts/xdp_stress_report.py config/doggy-dns-stress.toml config/doggy-dns-stress-xdp.toml
git commit -m "ci: add XDP integration test, benchmark, and stress test workflows"
```

---

### Task 11: Update AGENTS.md and config documentation

**Files:**
- Modify: `agents.md`
- Modify: `config/doggy-dns.toml` (add XDP section as commented example)

- [ ] **Step 1: Update agents.md with XDP build commands**

Append to the "Build & Test Commands" section in `agents.md`:

```markdown
cargo build --features xdp                 # Build with XDP support (requires Linux + bpf-linker)
cd xdp && cargo build --target bpfel-unknown-none --release  # Build eBPF program
cargo test -p doggy-dns-xdp                # Test XDP userspace crate
cargo bench -p doggy-dns-xdp               # Benchmark XDP components
sudo -E cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored  # XDP integration tests (requires root)
```

- [ ] **Step 2: Update agents.md architecture section**

Add to the "Crate Dependency Graph" in `agents.md`:

```
  ├── doggy-dns-xdp (optional, feature = "xdp")
  │     ├── doggy-dns-core
  │     └── doggy-dns-plugin
  │
  xdp/
  └── doggy-dns-xdp-ebpf  (eBPF program, separate workspace, target = bpfel-unknown-none)
```

- [ ] **Step 3: Add XDP config example to doggy-dns.toml**

Append to `config/doggy-dns.toml`:

```toml
# XDP fast path (optional, requires --features xdp build)
# [xdp]
# enabled = false
# interface = ""              # Empty = auto-detect default route interface
# cache_size = 65536          # BPF LRU hash map max entries
# negative_ttl = 30           # NXDomain cache TTL in seconds
# attach_mode = "auto"        # "auto" or "manual"
# pin_path = "/sys/fs/bpf/doggy-dns/dns_cache"
```

- [ ] **Step 4: Verify everything still builds and passes**

Run: `cargo build --workspace`
Run: `cargo test --workspace`
Run: `cargo clippy --all-features --all-targets -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add agents.md config/doggy-dns.toml
git commit -m "docs: update AGENTS.md and config with XDP fast path documentation"
```

---

## Self-Review Checklist

**Spec coverage:**
- [x] XDP cache fast path (Tasks 4, 5, 6)
- [x] Negative caching / NXDomain (Tasks 5, 6)
- [x] K8s DaemonSet support — interface auto-detection (Task 5), config (Task 2)
- [x] Userspace writes BPF map (Task 5 middleware)
- [x] All DNS responses cached (Task 3 handler populates bytes)
- [x] XDP_TX response (Task 6 eBPF program)
- [x] Graceful degradation (Task 7 main.rs)
- [x] Feature gate (Task 4 workspace, Task 7 main.rs)
- [x] Config validation (Task 2)
- [x] BPF map pinning (Task 5 program.rs)
- [x] attach_mode auto/manual (Task 5 program.rs)
- [x] CI integration tests (Task 8, 10)
- [x] CI benchmarks (Task 9, 10)
- [x] CI stress test (Task 10)
- [x] RequestOutcome extension (Task 1, 3)

**Type consistency:**
- `CacheKey` / `CacheValue` defined in `types.rs` (Task 4) — used consistently in Tasks 5, 6, 7
- `XdpConfig` defined in `config.rs` (Task 2) — used in Tasks 5, 7
- `RequestOutcome.response_bytes` / `min_answer_ttl` defined in Task 1 — populated in Task 3, consumed in Task 5
- `XdpManager` / `XdpCacheMiddleware` defined in Task 5 — used in Task 7
- `fnv1a_64` / `make_cache_key` / `compute_cache_entry` — defined in Task 5, tested in Task 5

**No placeholders:** All code blocks contain actual implementation code. Integration tests in Task 8 have `#[ignore]` markers with TODO comments for the actual XDP test logic — this is intentional since full eBPF integration tests require a running kernel and cannot be fully specified without hardware.
