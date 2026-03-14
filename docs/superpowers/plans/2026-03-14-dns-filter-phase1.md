# dns-filter Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust service-discovery DNS server on hickory-dns 0.26.0-beta.1 with a plugin middleware chain and Nacos as the first backend.

**Architecture:** Hybrid two-layer pipeline — a custom `RequestHandler` for outer middleware (logging, metrics), delegating to an `AuthorityChain` of `ZoneHandler` implementations (NacosAuthority → SystemAuthority → ForwardAuthority). Each authority uses `LookupControlFlow::Skip` to decline and pass to the next.

**Tech Stack:** Rust, hickory-dns 0.26.0-beta.1 (Server, ZoneHandler, TokioResolver), nacos-sdk 0.6, tokio, serde + toml, dashmap, arc-swap, tracing

**Spec:** `docs/superpowers/specs/2026-03-14-dns-filter-phase1-design.md`

---

## File Structure

```
dns-filter/
  Cargo.toml                                    # Workspace root (new)
  src/main.rs                                   # Binary entrypoint (new)
  config/dns-filter.toml                        # Example config (new)

  crates/dns-filter-core/Cargo.toml             # Core crate manifest (new)
  crates/dns-filter-core/src/lib.rs             # Core re-exports (new)
  crates/dns-filter-core/src/config.rs          # TOML config structs (new)
  crates/dns-filter-core/src/server.rs          # Server setup UDP/TCP/TLS/HTTPS (new)
  crates/dns-filter-core/src/handler.rs         # DnsFilterHandler: RequestHandler impl (new)
  crates/dns-filter-core/src/middleware/mod.rs   # Middleware trait + chain (new)
  crates/dns-filter-core/src/middleware/logging.rs  # Logging middleware (new)
  crates/dns-filter-core/src/middleware/metrics.rs  # Metrics middleware (new)

  crates/dns-filter-plugin/Cargo.toml           # Plugin crate manifest (new)
  crates/dns-filter-plugin/src/lib.rs           # Plugin re-exports (new)
  crates/dns-filter-plugin/src/plugin.rs        # Middleware Plugin trait (new)
  crates/dns-filter-plugin/src/authority.rs     # AuthorityPlugin factory trait (new)
  crates/dns-filter-plugin/src/authority_chain.rs  # AuthorityChain: Vec<Box<dyn ZoneHandler>> (new)
  crates/dns-filter-plugin/src/registry.rs      # Plugin registry from config (new)
  crates/dns-filter-plugin/src/builtin/mod.rs   # Built-in plugin re-exports (new)
  crates/dns-filter-plugin/src/builtin/system_dns.rs  # SystemAuthority (new)
  crates/dns-filter-plugin/src/builtin/forward.rs     # ForwardAuthority (new)

  crates/dns-filter-nacos/Cargo.toml            # Nacos crate manifest (new)
  crates/dns-filter-nacos/src/lib.rs            # Nacos re-exports (new)
  crates/dns-filter-nacos/src/authority.rs      # NacosAuthority: ZoneHandler impl (new)
  crates/dns-filter-nacos/src/watcher.rs        # Nacos service subscription + cache (new)
  crates/dns-filter-nacos/src/mapping.rs        # ServiceInstance → DNS Record (new)
  crates/dns-filter-nacos/src/config_watcher.rs # Nacos config listener for hot-reload (new)
```

---

## Chunk 1: Workspace Setup & Config Parsing

### Task 1: Initialize Cargo Workspace

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/dns-filter-core/Cargo.toml`
- Create: `crates/dns-filter-plugin/Cargo.toml`
- Create: `crates/dns-filter-nacos/Cargo.toml`
- Create: `src/main.rs`
- Create: `crates/dns-filter-core/src/lib.rs`
- Create: `crates/dns-filter-plugin/src/lib.rs`
- Create: `crates/dns-filter-nacos/src/lib.rs`

- [ ] **Step 1: Create workspace root Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "crates/dns-filter-core",
    "crates/dns-filter-plugin",
    "crates/dns-filter-nacos",
]

[package]
name = "dns-filter"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"
description = "A Rust-based service discovery DNS server built on hickory-dns"

[dependencies]
dns-filter-core = { path = "crates/dns-filter-core" }
dns-filter-plugin = { path = "crates/dns-filter-plugin" }
dns-filter-nacos = { path = "crates/dns-filter-nacos" }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"

[workspace.dependencies]
hickory-server = { version = "0.26.0-beta.1", features = ["tls-ring", "https-ring", "resolver"] }
hickory-proto = "0.26.0-beta.1"
hickory-resolver = { version = "0.26.0-beta.1", features = ["tokio", "system-config"] }
nacos-sdk = "0.6"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
toml = "1.0"
dashmap = "6"
arc-swap = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
async-trait = "0.1"
rustls = "0.23"
```

- [ ] **Step 2: Create dns-filter-plugin Cargo.toml** (fewest deps, foundation crate)

```toml
[package]
name = "dns-filter-plugin"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[dependencies]
hickory-server = { workspace = true }
hickory-proto = { workspace = true }
hickory-resolver = { workspace = true }
serde = { workspace = true }
toml = { workspace = true }
anyhow = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
arc-swap = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Create dns-filter-core Cargo.toml**

```toml
[package]
name = "dns-filter-core"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[dependencies]
dns-filter-plugin = { path = "../dns-filter-plugin" }
hickory-server = { workspace = true }
hickory-proto = { workspace = true }
serde = { workspace = true }
toml = { workspace = true }
tokio = { workspace = true }
anyhow = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
rustls = { workspace = true }
arc-swap = { workspace = true }

[dev-dependencies]
tracing-subscriber = { workspace = true }
```

- [ ] **Step 4: Create dns-filter-nacos Cargo.toml**

```toml
[package]
name = "dns-filter-nacos"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[dependencies]
dns-filter-plugin = { path = "../dns-filter-plugin" }
hickory-server = { workspace = true }
hickory-proto = { workspace = true }
nacos-sdk = { workspace = true }
serde = { workspace = true }
toml = { workspace = true }
tokio = { workspace = true }
dashmap = { workspace = true }
arc-swap = { workspace = true }
anyhow = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
```

- [ ] **Step 5: Create minimal lib.rs stubs for all crates**

`crates/dns-filter-core/src/lib.rs`:
```rust
pub mod config;
```

`crates/dns-filter-plugin/src/lib.rs`:
```rust
pub mod plugin;
pub mod authority;
pub mod authority_chain;
pub mod registry;
pub mod builtin;
```

`crates/dns-filter-nacos/src/lib.rs`:
```rust
pub mod authority;
pub mod watcher;
pub mod mapping;
pub mod config_watcher;
```

- [ ] **Step 6: Create minimal main.rs**

`src/main.rs`:
```rust
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dns_filter=info".into()),
        )
        .init();

    tracing::info!("dns-filter starting...");
    Ok(())
}
```

- [ ] **Step 7: Create empty module stubs** so the workspace compiles

Create all empty module files referenced by the lib.rs files. Each should contain just a comment:
```rust
// TODO: implementation
```

Files:
- `crates/dns-filter-core/src/config.rs`
- `crates/dns-filter-plugin/src/plugin.rs`
- `crates/dns-filter-plugin/src/authority.rs`
- `crates/dns-filter-plugin/src/authority_chain.rs`
- `crates/dns-filter-plugin/src/registry.rs`
- `crates/dns-filter-plugin/src/builtin/mod.rs`
- `crates/dns-filter-plugin/src/builtin/system_dns.rs`
- `crates/dns-filter-plugin/src/builtin/forward.rs`
- `crates/dns-filter-nacos/src/authority.rs`
- `crates/dns-filter-nacos/src/watcher.rs`
- `crates/dns-filter-nacos/src/mapping.rs`
- `crates/dns-filter-nacos/src/config_watcher.rs`

- [ ] **Step 8: Verify workspace compiles**

Run: `cargo check`
Expected: Compiles without errors (may have unused warnings)

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat: initialize cargo workspace with core, plugin, and nacos crates"
```

---

### Task 2: Config Parsing — Write Tests

**Files:**
- Create: `crates/dns-filter-core/src/config.rs`
- Create: `crates/dns-filter-core/tests/config_test.rs`

- [ ] **Step 1: Write failing tests for config parsing**

`crates/dns-filter-core/tests/config_test.rs`:
```rust
use dns_filter_core::config::{DnsFilterConfig, PluginConfig, PluginKind};

#[test]
fn parse_full_config() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
tcp_timeout = 10

[server.tls]
enabled = true
port = 853
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[server.https]
enabled = false
port = 443
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[middleware]
logging = true
metrics = false

[[plugins]]
kind = "nacos"
enabled = true
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "system_dns"
enabled = true
cache_size = 256
min_ttl = 10
max_ttl = 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53", "1.1.1.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600

[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DNS_FILTER_GROUP"
data_id = "dns-filter.toml"
"#;

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();

    assert_eq!(config.server.listen_addr, "0.0.0.0");
    assert_eq!(config.server.port, 53);
    assert_eq!(config.server.tcp_timeout, 10);

    let tls = config.server.tls.as_ref().unwrap();
    assert!(tls.enabled);
    assert_eq!(tls.port, 853);

    let https = config.server.https.as_ref().unwrap();
    assert!(!https.enabled);

    assert!(config.middleware.logging);
    assert!(!config.middleware.metrics);

    assert_eq!(config.plugins.len(), 3);

    let nacos = &config.plugins[0];
    assert_eq!(nacos.kind, PluginKind::Nacos);
    assert!(nacos.enabled);
    assert_eq!(nacos.server_addr.as_deref(), Some("127.0.0.1:8848"));
    assert_eq!(nacos.ttl, Some(6));

    let system_dns = &config.plugins[1];
    assert_eq!(system_dns.kind, PluginKind::SystemDns);
    assert_eq!(system_dns.cache_size, Some(256));
    assert_eq!(system_dns.min_ttl, Some(10));
    assert_eq!(system_dns.max_ttl, Some(300));

    let forward = &config.plugins[2];
    assert_eq!(forward.kind, PluginKind::Forward);
    let upstream = forward.upstream.as_ref().unwrap();
    assert_eq!(upstream.len(), 2);
    assert_eq!(upstream[0], "8.8.8.8:53");
}

#[test]
fn parse_minimal_config() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
"#;

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.port, 53);
    assert!(config.plugins.is_empty());
    assert!(!config.middleware.logging);
}

#[test]
fn defaults_for_optional_fields() {
    let toml_str = r#"
[server]
listen_addr = "127.0.0.1"
port = 5353
"#;

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.tcp_timeout, 10);
    assert!(config.server.tls.is_none());
    assert!(config.server.https.is_none());
    assert!(!config.remote_config.enabled);
}

#[test]
fn invalid_plugin_kind_fails() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[[plugins]]
kind = "unknown_plugin"
enabled = true
"#;

    let result: Result<DnsFilterConfig, _> = toml::from_str(toml_str);
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p dns-filter-core --test config_test`
Expected: FAIL — types do not exist yet

- [ ] **Step 3: Implement config structs**

`crates/dns-filter-core/src/config.rs`:
```rust
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct DnsFilterConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub middleware: MiddlewareConfig,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub remote_config: RemoteConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub port: u16,
    #[serde(default = "default_tcp_timeout")]
    pub tcp_timeout: u64,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout: u64,
    pub tls: Option<TlsConfig>,
    pub https: Option<HttpsConfig>,
}

fn default_tcp_timeout() -> u64 {
    10
}

fn default_shutdown_timeout() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tls_port")]
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
}

fn default_tls_port() -> u16 {
    853
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_https_port")]
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
}

fn default_https_port() -> u16 {
    443
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MiddlewareConfig {
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub metrics: bool,
    #[serde(default)]
    pub log_format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Nacos,
    SystemDns,
    Forward,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginConfig {
    pub kind: PluginKind,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(flatten)]
    pub settings: PluginSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PluginSettings {
    Nacos {
        server_addr: Option<String>,
        namespace: Option<String>,
        group: Option<String>,
        dns_zone: Option<String>,
        #[serde(default = "default_ttl")]
        ttl: u32,
    },
    Resolver {
        #[serde(default = "default_cache_size")]
        cache_size: u32,
        #[serde(default = "default_min_ttl")]
        min_ttl: u32,
        #[serde(default = "default_max_ttl")]
        max_ttl: u32,
        upstream: Option<Vec<String>>,
    },
}

fn default_enabled() -> bool { true }
fn default_ttl() -> u32 { 6 }
fn default_cache_size() -> u32 { 256 }
fn default_min_ttl() -> u32 { 10 }
fn default_max_ttl() -> u32 { 300 }

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub server_addr: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub data_id: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_addr: String::new(),
            namespace: String::new(),
            group: String::new(),
            data_id: String::new(),
        }
    }
}

/// Load and validate config from a TOML file path.
pub fn load_config(path: &str) -> anyhow::Result<DnsFilterConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: DnsFilterConfig = toml::from_str(&content)?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &DnsFilterConfig) -> anyhow::Result<()> {
    if let Some(tls) = &config.server.tls {
        if tls.enabled {
            anyhow::ensure!(
                std::path::Path::new(&tls.cert_path).exists(),
                "TLS cert_path does not exist: {}", tls.cert_path
            );
            anyhow::ensure!(
                std::path::Path::new(&tls.key_path).exists(),
                "TLS key_path does not exist: {}", tls.key_path
            );
        }
    }
    if let Some(https) = &config.server.https {
        if https.enabled {
            anyhow::ensure!(
                std::path::Path::new(&https.cert_path).exists(),
                "HTTPS cert_path does not exist: {}", https.cert_path
            );
            anyhow::ensure!(
                std::path::Path::new(&https.key_path).exists(),
                "HTTPS key_path does not exist: {}", https.key_path
            );
        }
    }
    if config.remote_config.enabled {
        anyhow::ensure!(
            !config.remote_config.server_addr.is_empty(),
            "remote_config.server_addr is required when remote_config is enabled"
        );
        anyhow::ensure!(
            !config.remote_config.data_id.is_empty(),
            "remote_config.data_id is required when remote_config is enabled"
        );
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p dns-filter-core --test config_test`
Expected: All 4 tests PASS

- [ ] **Step 5: Create example config file**

`config/dns-filter.toml`:
```toml
[server]
listen_addr = "0.0.0.0"
port = 53
tcp_timeout = 10

#[server.tls]
#enabled = false
#port = 853
#cert_path = "/path/to/cert.pem"
#key_path = "/path/to/key.pem"

#[server.https]
#enabled = false
#port = 443
#cert_path = "/path/to/cert.pem"
#key_path = "/path/to/key.pem"

[middleware]
logging = true
metrics = false

[[plugins]]
kind = "nacos"
enabled = true
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "system_dns"
enabled = true
cache_size = 256
min_ttl = 10
max_ttl = 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53", "1.1.1.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600

[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DNS_FILTER_GROUP"
data_id = "dns-filter.toml"
```

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: add TOML config parsing with tests"
```

---

## Chunk 2: Plugin Traits & Authority Chain

### Task 3: Plugin Trait Definitions

**Files:**
- Create: `crates/dns-filter-plugin/src/plugin.rs`
- Create: `crates/dns-filter-plugin/src/authority.rs`

- [ ] **Step 1: Define the Middleware Plugin trait**

`crates/dns-filter-plugin/src/plugin.rs`:
```rust
use async_trait::async_trait;
use hickory_server::server::Request;

/// Result of middleware processing.
pub enum MiddlewareAction {
    /// Continue to the next middleware / authority chain.
    Continue,
    /// Short-circuit: skip remaining middleware and authority chain.
    /// The middleware has already sent the DNS response.
    ShortCircuit,
}

/// Cross-cutting middleware plugin (logging, metrics, etc.).
/// Wraps request processing with before/after hooks.
#[async_trait]
pub trait Middleware: Send + Sync {
    /// Called before the authority chain processes the request.
    /// Return `ShortCircuit` to skip the authority chain.
    async fn before_request(&self, request: &Request) -> MiddlewareAction;

    /// Called after the authority chain has processed the request.
    async fn after_request(&self, request: &Request, duration_ms: u64);
}
```

- [ ] **Step 2: Define the AuthorityPlugin factory trait**

`crates/dns-filter-plugin/src/authority.rs`:
```rust
use anyhow::Result;
use hickory_server::zone_handler::ZoneHandler;
use std::sync::Arc;

/// Factory trait for creating a ZoneHandler from a plugin config section.
/// Each backend crate (nacos, system_dns, forward) implements this trait.
pub trait AuthorityPlugin: Send + Sync {
    /// Create a ZoneHandler from config values.
    fn create_zone_handler(&self) -> Result<Arc<dyn ZoneHandler>>;
}
```

- [ ] **Step 3: Update lib.rs to re-export**

`crates/dns-filter-plugin/src/lib.rs`:
```rust
pub mod plugin;
pub mod authority;
pub mod authority_chain;
pub mod registry;
pub mod builtin;

pub use plugin::{Middleware, MiddlewareAction};
pub use authority::AuthorityPlugin;
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p dns-filter-plugin`
Expected: Compiles (check that `hickory_server::zone_handler::ZoneHandler` exists in 0.26)

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: define Middleware and AuthorityPlugin traits"
```

---

### Task 4: AuthorityChain

**Files:**
- Create: `crates/dns-filter-plugin/src/authority_chain.rs`
- Create: `crates/dns-filter-plugin/tests/authority_chain_test.rs`

- [ ] **Step 1: Write failing test for authority chain**

`crates/dns-filter-plugin/tests/authority_chain_test.rs`:
```rust
use dns_filter_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Header, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, RecordType};
use hickory_server::zone_handler::LookupControlFlow;
use std::str::FromStr;
use std::sync::Arc;

// NOTE: A real test will need a mock ZoneHandler.
// This test verifies the chain returns Skip when empty.
#[tokio::test]
async fn empty_chain_returns_skip() {
    let chain = AuthorityChain::new(vec![]);
    let name = Name::from_str("example.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result, LookupControlFlow::Skip));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p dns-filter-plugin --test authority_chain_test`
Expected: FAIL — `AuthorityChain` does not exist

- [ ] **Step 3: Implement AuthorityChain**

`crates/dns-filter-plugin/src/authority_chain.rs`:
```rust
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::server::RequestInfo;
use hickory_server::zone_handler::{AuthLookup, LookupControlFlow, LookupOptions, ZoneHandler};
use std::sync::Arc;

/// Iterates through a chain of ZoneHandlers.
/// Stops at the first handler that returns Continue or Break (not Skip).
pub struct AuthorityChain {
    handlers: Vec<Arc<dyn ZoneHandler>>,
}

impl AuthorityChain {
    pub fn new(handlers: Vec<Arc<dyn ZoneHandler>>) -> Self {
        Self { handlers }
    }

    /// Resolve a query by iterating the chain.
    /// Returns the first non-Skip result, or Skip if all handlers skip.
    pub async fn resolve(
        &self,
        name: &Name,
        record_type: RecordType,
        request_info: Option<&RequestInfo<'_>>,
    ) -> LookupControlFlow<AuthLookup> {
        let lower_name = LowerName::new(name);
        let lookup_options = LookupOptions::default();

        for handler in &self.handlers {
            let result = handler
                .lookup(&lower_name, record_type, request_info, lookup_options)
                .await;

            match &result {
                LookupControlFlow::Skip => continue,
                LookupControlFlow::Continue(_) | LookupControlFlow::Break(_) => return result,
            }
        }

        LookupControlFlow::Skip
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p dns-filter-plugin --test authority_chain_test`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add AuthorityChain for iterating ZoneHandlers"
```

---

### Task 5: ForwardAuthority (built-in)

**Files:**
- Create: `crates/dns-filter-plugin/src/builtin/forward.rs`
- Create: `crates/dns-filter-plugin/src/builtin/mod.rs`
- Create: `crates/dns-filter-plugin/tests/forward_test.rs`

- [ ] **Step 1: Write failing test for ForwardAuthority**

`crates/dns-filter-plugin/tests/forward_test.rs`:
```rust
use dns_filter_plugin::builtin::forward::ForwardAuthority;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::time::Duration;

#[tokio::test]
async fn forward_resolves_known_domain() {
    let authority = ForwardAuthority::new(
        vec!["8.8.8.8:53".to_string()],
        1024,
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await
    .expect("failed to create ForwardAuthority");

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_)) => {
            // Success: resolved
        }
        other => panic!("Expected successful lookup, got: {:?}", other),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p dns-filter-plugin --test forward_test`
Expected: FAIL — `ForwardAuthority` does not exist

- [ ] **Step 3: Implement ForwardAuthority**

`crates/dns-filter-plugin/src/builtin/mod.rs`:
```rust
pub mod forward;
pub mod system_dns;
```

`crates/dns-filter-plugin/src/builtin/forward.rs`:
```rust
use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::rr::{LowerName, Name, Record, RecordType};
use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::Resolver;
use hickory_resolver::name_server::TokioRuntimeProvider;
use hickory_server::server::RequestInfo;
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupError, LookupOptions,
    ZoneHandler, ZoneType,
};
use hickory_server::server::Request;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub struct ForwardAuthority {
    resolver: Resolver<TokioRuntimeProvider>,
    origin: LowerName,
}

impl ForwardAuthority {
    pub async fn new(
        upstream: Vec<String>,
        cache_size: u64,
        min_ttl: Duration,
        max_ttl: Duration,
    ) -> Result<Self> {
        let name_servers: Vec<NameServerConfig> = upstream
            .iter()
            .map(|addr| {
                let socket_addr: SocketAddr = addr.parse()
                    .unwrap_or_else(|_| format!("{}:53", addr).parse().unwrap());
                NameServerConfig::new(socket_addr, hickory_proto::xfer::Protocol::Udp)
            })
            .collect();

        let config = ResolverConfig::from_parts(None, vec![], name_servers);

        let mut opts = ResolverOpts::default();
        opts.cache_size = cache_size as usize;
        opts.positive_min_ttl = Some(min_ttl);
        opts.positive_max_ttl = Some(max_ttl);

        let resolver = Resolver::builder_with_config(config, TokioRuntimeProvider::default())
            .with_options(opts)
            .build()?;

        let origin = LowerName::from(Name::root());
        Ok(Self { resolver, origin })
    }
}

#[async_trait]
impl ZoneHandler for ForwardAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Forward
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let name_str = name.to_string();
        match self.resolver.lookup(&name_str, rtype).await {
            Ok(lookup) => {
                let records: Vec<Record> = lookup.record_iter().cloned().collect();
                if records.is_empty() {
                    LookupControlFlow::Skip
                } else {
                    LookupControlFlow::Continue(Ok(AuthLookup::answers(
                        records.into(),
                        None,
                    )))
                }
            }
            Err(_) => LookupControlFlow::Skip,
        }
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<hickory_proto::op::tsig::TSigResponseContext>) {
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(_) => return (LookupControlFlow::Skip, None),
        };
        let result = self
            .lookup(
                &request_info.query.name().into(),
                request_info.query.query_type(),
                Some(&request_info),
                lookup_options,
            )
            .await;
        (result, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p dns-filter-plugin --test forward_test`
Expected: PASS (requires network access to reach 8.8.8.8)

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add ForwardAuthority wrapping hickory-resolver"
```

---

### Task 6: SystemAuthority (built-in)

**Files:**
- Create: `crates/dns-filter-plugin/src/builtin/system_dns.rs`
- Create: `crates/dns-filter-plugin/tests/system_dns_test.rs`

- [ ] **Step 1: Write failing test for SystemAuthority**

`crates/dns-filter-plugin/tests/system_dns_test.rs`:
```rust
use dns_filter_plugin::builtin::system_dns::SystemAuthority;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::time::Duration;

#[tokio::test]
async fn system_dns_resolves_known_domain() {
    let authority = SystemAuthority::new(
        256,
        Duration::from_secs(10),
        Duration::from_secs(300),
    )
    .await
    .expect("failed to create SystemAuthority");

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_)) => {
            // Success
        }
        other => panic!("Expected successful lookup, got: {:?}", other),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p dns-filter-plugin --test system_dns_test`
Expected: FAIL — `SystemAuthority` does not exist

- [ ] **Step 3: Implement SystemAuthority**

`crates/dns-filter-plugin/src/builtin/system_dns.rs`:
```rust
use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::rr::{LowerName, Name, Record, RecordType};
use hickory_resolver::Resolver;
use hickory_resolver::config::ResolverOpts;
use hickory_resolver::name_server::TokioRuntimeProvider;
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions,
    ZoneHandler, ZoneType,
};
use std::time::Duration;

pub struct SystemAuthority {
    resolver: Resolver<TokioRuntimeProvider>,
    origin: LowerName,
}

impl SystemAuthority {
    pub async fn new(
        cache_size: u64,
        min_ttl: Duration,
        max_ttl: Duration,
    ) -> Result<Self> {
        let mut opts = ResolverOpts::default();
        opts.cache_size = cache_size as usize;
        opts.positive_min_ttl = Some(min_ttl);
        opts.positive_max_ttl = Some(max_ttl);

        // Reads /etc/resolv.conf on Unix
        let mut builder = Resolver::builder_tokio()?;
        *builder.options_mut() = opts;
        let resolver = builder.build()?;

        let origin = LowerName::from(Name::root());
        Ok(Self { resolver, origin })
    }
}

#[async_trait]
impl ZoneHandler for SystemAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Forward
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let name_str = name.to_string();
        match self.resolver.lookup(&name_str, rtype).await {
            Ok(lookup) => {
                let records: Vec<Record> = lookup.record_iter().cloned().collect();
                if records.is_empty() {
                    LookupControlFlow::Skip
                } else {
                    LookupControlFlow::Continue(Ok(AuthLookup::answers(
                        records.into(),
                        None,
                    )))
                }
            }
            Err(_) => LookupControlFlow::Skip,
        }
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<hickory_proto::op::tsig::TSigResponseContext>) {
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(_) => return (LookupControlFlow::Skip, None),
        };
        let result = self
            .lookup(
                &request_info.query.name().into(),
                request_info.query.query_type(),
                Some(&request_info),
                lookup_options,
            )
            .await;
        (result, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p dns-filter-plugin --test system_dns_test`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add SystemAuthority wrapping system /etc/resolv.conf"
```

---

## Chunk 3: Nacos Integration

### Task 7: Nacos DNS Record Mapping

**Files:**
- Create: `crates/dns-filter-nacos/src/mapping.rs`
- Create: `crates/dns-filter-nacos/tests/mapping_test.rs`

- [ ] **Step 1: Write failing tests for mapping**

`crates/dns-filter-nacos/tests/mapping_test.rs`:
```rust
use dns_filter_nacos::mapping::{parse_dns_name, DnsNameParts, to_dns_records};
use hickory_proto::rr::{Name, RecordType};
use std::net::Ipv4Addr;
use std::str::FromStr;

#[test]
fn parse_valid_dns_name() {
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let result = parse_dns_name(&name, "nacos.local");
    assert!(result.is_some());
    let parts = result.unwrap();
    assert_eq!(parts.service_name, "user-service");
    assert_eq!(parts.group, "DEFAULT_GROUP");
    assert_eq!(parts.namespace, "public");
}

#[test]
fn parse_invalid_dns_name_wrong_zone() {
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.other.local.").unwrap();
    let result = parse_dns_name(&name, "nacos.local");
    assert!(result.is_none());
}

#[test]
fn parse_invalid_dns_name_too_few_labels() {
    let name = Name::from_str("user-service.nacos.local.").unwrap();
    let result = parse_dns_name(&name, "nacos.local");
    assert!(result.is_none());
}

#[test]
fn to_dns_records_creates_a_records() {
    let ips = vec!["10.0.1.5".to_string(), "10.0.1.6".to_string()];
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6);

    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(record.ttl(), 6);
    }
}

#[test]
fn to_dns_records_handles_ipv6() {
    let ips = vec!["::1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_skips_invalid_ips() {
    let ips = vec!["not-an-ip".to_string(), "10.0.1.1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6);

    assert_eq!(records.len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p dns-filter-nacos --test mapping_test`
Expected: FAIL — module does not exist

- [ ] **Step 3: Implement mapping module**

`crates/dns-filter-nacos/src/mapping.rs`:
```rust
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsNameParts {
    pub service_name: String,
    pub group: String,
    pub namespace: String,
}

/// Parse a DNS query name into service parts.
/// Expected format: `<service>.<group>.<namespace>.<zone_suffix>.`
/// Returns None if the name doesn't match the expected zone.
pub fn parse_dns_name(name: &Name, zone_suffix: &str) -> Option<DnsNameParts> {
    let name_str = name.to_ascii();
    let name_str = name_str.trim_end_matches('.');

    let suffix = zone_suffix.trim_end_matches('.');
    if !name_str.ends_with(suffix) {
        return None;
    }

    let prefix = name_str[..name_str.len() - suffix.len()]
        .trim_end_matches('.');

    let parts: Vec<&str> = prefix.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    Some(DnsNameParts {
        service_name: parts[0].to_string(),
        group: parts[1].to_string(),
        namespace: parts[2].to_string(),
    })
}

/// Convert a list of IP strings to DNS A/AAAA records.
/// Invalid IPs are skipped with a warning log.
pub fn to_dns_records(name: &Name, ips: &[String], ttl: u32) -> Vec<Record> {
    ips.iter()
        .filter_map(|ip_str| {
            match IpAddr::from_str(ip_str) {
                Ok(IpAddr::V4(ipv4)) => {
                    let record = Record::from_rdata(name.clone(), ttl, RData::A(A(ipv4)));
                    Some(record)
                }
                Ok(IpAddr::V6(ipv6)) => {
                    let record = Record::from_rdata(name.clone(), ttl, RData::AAAA(AAAA(ipv6)));
                    Some(record)
                }
                Err(e) => {
                    tracing::warn!("skipping invalid IP '{}': {}", ip_str, e);
                    None
                }
            }
        })
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p dns-filter-nacos --test mapping_test`
Expected: All 5 tests PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add Nacos DNS name parsing and record mapping"
```

---

### Task 8: Nacos Service Watcher

**Files:**
- Create: `crates/dns-filter-nacos/src/watcher.rs`

- [ ] **Step 1: Implement NacosServiceWatcher**

`crates/dns-filter-nacos/src/watcher.rs`:
```rust
use dashmap::DashMap;
use nacos_sdk::api::naming::{
    NamingChangeEvent, NamingEventListener, NamingService, NamingServiceBuilder, ServiceInstance,
};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;
use tracing;

/// Key for the service instance cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceKey {
    pub service_name: String,
    pub group: String,
    pub namespace: String,
}

/// Watches Nacos for service instance changes and maintains an in-memory cache.
pub struct NacosServiceWatcher {
    naming_service: Arc<NamingService>,
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    namespace: String,
    group: String,
}

/// Simplified service instance for DNS resolution.
#[derive(Debug, Clone)]
pub struct CachedInstance {
    pub ip: String,
    pub port: i32,
    pub healthy: bool,
}

impl From<&ServiceInstance> for CachedInstance {
    fn from(instance: &ServiceInstance) -> Self {
        Self {
            ip: instance.ip.clone(),
            port: instance.port,
            healthy: instance.healthy,
        }
    }
}

struct InstanceChangeListener {
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    group: String,
    namespace: String,
}

impl NamingEventListener for InstanceChangeListener {
    fn event(&self, event: Arc<NamingChangeEvent>) {
        let service_name = event.service_name.clone();
        tracing::info!("nacos service changed: {}", service_name);

        let key = ServiceKey {
            service_name,
            group: self.group.clone(),
            namespace: self.namespace.clone(),
        };

        match &event.instances {
            Some(instances) => {
                let cached: Vec<CachedInstance> = instances
                    .iter()
                    .filter(|i| i.healthy && i.enabled)
                    .map(CachedInstance::from)
                    .collect();
                // Immutable replacement: insert replaces the entire value
                self.cache.insert(key, cached);
            }
            None => {
                self.cache.remove(&key);
            }
        }
    }
}

impl NacosServiceWatcher {
    pub async fn new(
        server_addr: &str,
        namespace: &str,
        group: &str,
    ) -> anyhow::Result<Self> {
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name("dns-filter");

        let naming_service = NamingServiceBuilder::new(props).build()?;

        Ok(Self {
            naming_service: Arc::new(naming_service),
            cache: Arc::new(DashMap::new()),
            namespace: namespace.to_string(),
            group: group.to_string(),
        })
    }

    /// Subscribe to a specific service for push updates.
    pub async fn subscribe(&self, service_name: &str) {
        let listener = InstanceChangeListener {
            cache: Arc::clone(&self.cache),
            group: self.group.clone(),
            namespace: self.namespace.clone(),
        };

        self.naming_service
            .subscribe(
                service_name.to_string(),
                Some(self.group.clone()),
                vec![],
                Arc::new(listener),
            )
            .await;

        tracing::info!("subscribed to nacos service: {}", service_name);
    }

    /// Get cached instances for a service.
    pub fn get_instances(&self, key: &ServiceKey) -> Option<Vec<CachedInstance>> {
        self.cache.get(key).map(|entry| entry.value().clone())
    }

    /// Subscribe to all services in the configured namespace/group.
    /// Fetches the current service list and subscribes to each.
    pub async fn subscribe_all(&self) {
        match self.naming_service.get_all_instances(
            String::new(), // empty = all services
            Some(self.group.clone()),
            vec![],
            false,
        ).await {
            Ok(instances) => {
                let mut seen_services = std::collections::HashSet::new();
                for instance in &instances {
                    if seen_services.insert(instance.service_name.clone()) {
                        self.subscribe(&instance.service_name).await;
                    }
                }
                tracing::info!("subscribed to {} nacos services", seen_services.len());
            }
            Err(e) => {
                tracing::warn!("failed to list nacos services: {}, will rely on dynamic subscriptions", e);
            }
        }
    }

    /// Get a reference to the cache.
    pub fn cache(&self) -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
        Arc::clone(&self.cache)
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p dns-filter-nacos`
Expected: Compiles (nacos-sdk types may need adjustment based on 0.6 API)

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: add NacosServiceWatcher with DashMap cache"
```

---

### Task 9: NacosAuthority (ZoneHandler impl)

**Files:**
- Create: `crates/dns-filter-nacos/src/authority.rs`
- Create: `crates/dns-filter-nacos/tests/authority_test.rs`

- [ ] **Step 1: Write failing test for NacosAuthority**

`crates/dns-filter-nacos/tests/authority_test.rs`:
```rust
use dashmap::DashMap;
use dns_filter_nacos::authority::NacosAuthority;
use dns_filter_nacos::watcher::{CachedInstance, ServiceKey};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::sync::Arc;

fn make_test_cache() -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "user-service".to_string(),
            group: "DEFAULT_GROUP".to_string(),
            namespace: "public".to_string(),
        },
        vec![
            CachedInstance {
                ip: "10.0.1.5".to_string(),
                port: 8080,
                healthy: true,
            },
            CachedInstance {
                ip: "10.0.1.6".to_string(),
                port: 8080,
                healthy: true,
            },
        ],
    );
    cache
}

#[tokio::test]
async fn resolves_known_service() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

    let name = LowerName::from(
        Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap(),
    );

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            let records: Vec<_> = lookup.iter().collect();
            assert_eq!(records.len(), 2);
        }
        other => panic!("Expected Continue(Ok), got: {:?}", other),
    }
}

#[tokio::test]
async fn skips_unknown_service() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

    let name = LowerName::from(
        Name::from_str("unknown.DEFAULT_GROUP.public.nacos.local.").unwrap(),
    );

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
}

#[tokio::test]
async fn skips_wrong_zone() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p dns-filter-nacos --test authority_test`
Expected: FAIL — `NacosAuthority` does not exist

- [ ] **Step 3: Implement NacosAuthority**

`crates/dns-filter-nacos/src/authority.rs`:
```rust
use crate::mapping::{parse_dns_name, to_dns_records};
use crate::watcher::{CachedInstance, ServiceKey};
use async_trait::async_trait;
use dashmap::DashMap;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::sync::Arc;

pub struct NacosAuthority {
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    dns_zone: String,
    ttl: u32,
    origin: LowerName,
}

impl NacosAuthority {
    pub fn new(
        cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
        dns_zone: &str,
        ttl: u32,
    ) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let origin = LowerName::from(Name::from_ascii(&zone_name).unwrap_or_default());
        Self {
            cache,
            dns_zone: dns_zone.to_string(),
            ttl,
            origin,
        }
    }
}

#[async_trait]
impl ZoneHandler for NacosAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        if !matches!(rtype, RecordType::A | RecordType::AAAA) {
            return LookupControlFlow::Skip;
        }

        let query_name = Name::from(name.clone());
        let parts = match parse_dns_name(&query_name, &self.dns_zone) {
            Some(p) => p,
            None => return LookupControlFlow::Skip,
        };

        let key = ServiceKey {
            service_name: parts.service_name,
            group: parts.group,
            namespace: parts.namespace,
        };

        let instances = match self.cache.get(&key) {
            Some(entry) => entry.value().clone(),
            None => return LookupControlFlow::Skip,
        };

        let ips: Vec<String> = instances.iter().map(|i| i.ip.clone()).collect();
        let records = to_dns_records(&query_name, &ips, self.ttl);

        if records.is_empty() {
            return LookupControlFlow::Skip;
        }

        LookupControlFlow::Continue(Ok(AuthLookup::answers(records.into(), None)))
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<hickory_proto::op::tsig::TSigResponseContext>) {
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(_) => return (LookupControlFlow::Skip, None),
        };
        let result = self
            .lookup(
                &request_info.query.name().into(),
                request_info.query.query_type(),
                Some(&request_info),
                lookup_options,
            )
            .await;
        (result, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p dns-filter-nacos --test authority_test`
Expected: All 3 tests PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add NacosAuthority with ZoneHandler implementation"
```

---

## Chunk 4: RequestHandler, Middleware, & Server

### Task 10: Logging and Metrics Middleware

**Files:**
- Create: `crates/dns-filter-core/src/middleware/mod.rs`
- Create: `crates/dns-filter-core/src/middleware/logging.rs`
- Create: `crates/dns-filter-core/src/middleware/metrics.rs`

- [ ] **Step 1: Create middleware mod.rs**

`crates/dns-filter-core/src/middleware/mod.rs`:
```rust
pub mod logging;
pub mod metrics;
```

- [ ] **Step 2: Implement LoggingMiddleware**

`crates/dns-filter-core/src/middleware/logging.rs`:
```rust
use async_trait::async_trait;
use dns_filter_plugin::{Middleware, MiddlewareAction};
use hickory_server::server::Request;

pub struct LoggingMiddleware;

#[async_trait]
impl Middleware for LoggingMiddleware {
    async fn before_request(&self, request: &Request) -> MiddlewareAction {
        if let Ok(info) = request.request_info() {
            tracing::info!(
                src = %info.src,
                query_name = %info.query.name(),
                query_type = ?info.query.query_type(),
                protocol = ?info.protocol,
                "dns query received"
            );
        }
        MiddlewareAction::Continue
    }

    async fn after_request(&self, request: &Request, duration_ms: u64) {
        if let Ok(info) = request.request_info() {
            tracing::info!(
                query_name = %info.query.name(),
                duration_ms = duration_ms,
                "dns query completed"
            );
        }
    }
}
```

- [ ] **Step 3: Implement MetricsMiddleware**

`crates/dns-filter-core/src/middleware/metrics.rs`:
```rust
use async_trait::async_trait;
use dns_filter_plugin::{Middleware, MiddlewareAction};
use hickory_server::server::Request;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct MetricsMiddleware {
    pub queries_total: AtomicU64,
}

impl MetricsMiddleware {
    pub fn new() -> Self {
        Self {
            queries_total: AtomicU64::new(0),
        }
    }

    pub fn total_queries(&self) -> u64 {
        self.queries_total.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Middleware for MetricsMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        self.queries_total.fetch_add(1, Ordering::Relaxed);
        MiddlewareAction::Continue
    }

    async fn after_request(&self, _request: &Request, _duration_ms: u64) {
        // Phase 1: just count. Histogram deferred.
    }
}
```

- [ ] **Step 4: Update dns-filter-core/src/lib.rs**

```rust
pub mod config;
pub mod middleware;
pub mod handler;
pub mod server;
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p dns-filter-core`
Expected: Compiles

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: add logging and metrics middleware"
```

---

### Task 11: DnsFilterHandler (RequestHandler)

**Files:**
- Create: `crates/dns-filter-core/src/handler.rs`
- Create: `crates/dns-filter-core/tests/handler_test.rs`

- [ ] **Step 1: Implement DnsFilterHandler**

`crates/dns-filter-core/src/handler.rs`:
```rust
use async_trait::async_trait;
use dns_filter_plugin::authority_chain::AuthorityChain;
use dns_filter_plugin::{Middleware, MiddlewareAction};
use hickory_proto::op::{Header, MessageType, OpCode, ResponseCode};
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{LookupControlFlow, MessageResponseBuilder};
use std::sync::Arc;
use std::time::Instant;

pub struct DnsFilterHandler {
    middlewares: Vec<Arc<dyn Middleware>>,
    authority_chain: Arc<AuthorityChain>,
}

impl DnsFilterHandler {
    pub fn new(
        middlewares: Vec<Arc<dyn Middleware>>,
        authority_chain: Arc<AuthorityChain>,
    ) -> Self {
        Self {
            middlewares,
            authority_chain,
        }
    }
}

#[async_trait]
impl RequestHandler for DnsFilterHandler {
    async fn handle_request<R: ResponseHandler, T: hickory_server::server::Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let start = Instant::now();

        // Run before_request on all middlewares
        for mw in &self.middlewares {
            if let MiddlewareAction::ShortCircuit = mw.before_request(request).await {
                let duration_ms = start.elapsed().as_millis() as u64;
                for mw in &self.middlewares {
                    mw.after_request(request, duration_ms).await;
                }
                // Return a REFUSED response for short-circuited requests
                return send_error(&request, &mut response_handle, ResponseCode::Refused).await;
            }
        }

        // Extract query info
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(_) => {
                return send_error(&request, &mut response_handle, ResponseCode::FormErr).await;
            }
        };

        // Resolve through authority chain
        let query_name = request_info.query.name().to_owned();
        let query_type = request_info.query.query_type();
        let result = self
            .authority_chain
            .resolve(&query_name.into(), query_type, Some(&request_info))
            .await;

        let response_info = match result {
            LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
                let builder = MessageResponseBuilder::from_message_request(request);
                let mut header = Header::response_from_request(request.header());
                header.set_message_type(MessageType::Response);
                header.set_op_code(OpCode::Query);
                header.set_response_code(ResponseCode::NoError);

                let records: Vec<_> = lookup.iter().cloned().collect();
                let response = builder.build(header, records.iter(), &[], &[], &[]);

                match response_handle.send_response(response).await {
                    Ok(info) => info,
                    Err(_) => send_error(request, &mut response_handle, ResponseCode::ServFail).await,
                }
            }
            LookupControlFlow::Continue(Err(_)) | LookupControlFlow::Break(Err(_)) => {
                send_error(request, &mut response_handle, ResponseCode::ServFail).await
            }
            LookupControlFlow::Skip => {
                // No authority handled the query
                send_error(request, &mut response_handle, ResponseCode::NXDomain).await
            }
        };

        // Run after_request on all middlewares
        let duration_ms = start.elapsed().as_millis() as u64;
        for mw in &self.middlewares {
            mw.after_request(request, duration_ms).await;
        }

        response_info
    }
}

async fn send_error<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    response_code: ResponseCode,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let response = builder.error_msg(request.header(), response_code);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(_) => {
            let mut header = Header::new();
            header.set_response_code(ResponseCode::ServFail);
            header.into()
        }
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p dns-filter-core`
Expected: Compiles (exact API may need adjustment based on 0.26 types)

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: add DnsFilterHandler implementing RequestHandler"
```

---

### Task 12: Server Setup (UDP/TCP/TLS/HTTPS)

**Files:**
- Create: `crates/dns-filter-core/src/server.rs`

- [ ] **Step 1: Implement server setup**

`crates/dns-filter-core/src/server.rs`:
```rust
use crate::config::{DnsFilterConfig, HttpsConfig, TlsConfig};
use anyhow::{Context, Result};
use hickory_server::Server;
use hickory_server::server::RequestHandler;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};

pub async fn build_server<H: RequestHandler>(
    config: &DnsFilterConfig,
    handler: H,
) -> Result<Server<H>> {
    let mut server = Server::new(handler);

    // UDP
    let udp_addr: SocketAddr = format!("{}:{}", config.server.listen_addr, config.server.port)
        .parse()
        .context("invalid server address")?;
    let udp_socket = UdpSocket::bind(udp_addr)
        .await
        .context("failed to bind UDP socket")?;
    server.register_socket(udp_socket.into_std()?);
    tracing::info!("listening on UDP {}", udp_addr);

    // TCP
    let tcp_listener = TcpListener::bind(udp_addr)
        .await
        .context("failed to bind TCP listener")?;
    let tcp_timeout = Duration::from_secs(config.server.tcp_timeout);
    server.register_listener(tcp_listener.into_std()?, tcp_timeout);
    tracing::info!("listening on TCP {}", udp_addr);

    // TLS (DoT)
    if let Some(tls_config) = &config.server.tls {
        if tls_config.enabled {
            register_tls(&mut server, config, tls_config).await?;
        }
    }

    // HTTPS (DoH)
    if let Some(https_config) = &config.server.https {
        if https_config.enabled {
            register_https(&mut server, config, https_config).await?;
        }
    }

    Ok(server)
}

async fn register_tls<H: RequestHandler>(
    server: &mut Server<H>,
    config: &DnsFilterConfig,
    tls_config: &TlsConfig,
) -> Result<()> {
    let tls_addr: SocketAddr =
        format!("{}:{}", config.server.listen_addr, tls_config.port).parse()?;

    let cert_pem = fs::read(&tls_config.cert_path)
        .context("failed to read TLS certificate")?;
    let key_pem = fs::read(&tls_config.key_path)
        .context("failed to read TLS private key")?;

    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .context("failed to parse TLS certificate")?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("failed to parse TLS private key")?
        .context("no private key found")?;

    let tls_server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build TLS server config")?;

    let listener = TcpListener::bind(tls_addr).await?;
    let timeout = Duration::from_secs(config.server.tcp_timeout);

    server.register_tls_listener_with_tls_config(
        listener.into_std()?,
        timeout,
        Arc::new(tls_server_config),
    )?;

    tracing::info!("listening on TLS {}", tls_addr);
    Ok(())
}

async fn register_https<H: RequestHandler>(
    server: &mut Server<H>,
    config: &DnsFilterConfig,
    https_config: &HttpsConfig,
) -> Result<()> {
    let https_addr: SocketAddr =
        format!("{}:{}", config.server.listen_addr, https_config.port).parse()?;

    let cert_pem = fs::read(&https_config.cert_path)
        .context("failed to read HTTPS certificate")?;
    let key_pem = fs::read(&https_config.key_path)
        .context("failed to read HTTPS private key")?;

    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .context("failed to parse HTTPS certificate")?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("failed to parse HTTPS private key")?
        .context("no private key found")?;

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls_config.alpn_protocols = vec![b"h2".to_vec()];

    let listener = TcpListener::bind(https_addr).await?;
    let timeout = Duration::from_secs(config.server.tcp_timeout);

    server.register_https_listener_with_tls_config(
        listener.into_std()?,
        timeout,
        Arc::new(tls_config),
        None,
        "/dns-query".to_string(),
    )?;

    tracing::info!("listening on HTTPS {}", https_addr);
    Ok(())
}
```

- [ ] **Step 2: Add rustls-pemfile dependency to dns-filter-core**

Add to `crates/dns-filter-core/Cargo.toml` under `[dependencies]`:
```toml
rustls-pemfile = "2"
```
And add to workspace `Cargo.toml` under `[workspace.dependencies]`:
```toml
rustls-pemfile = "2"
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p dns-filter-core`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: add server setup with UDP/TCP/TLS/HTTPS listeners"
```

---

### Task 13: Unit Tests for Middleware, Handler, and Watcher

**Files:**
- Create: `crates/dns-filter-core/tests/middleware_test.rs`
- Create: `crates/dns-filter-core/tests/handler_test.rs`

- [ ] **Step 1: Write tests for MetricsMiddleware**

`crates/dns-filter-core/tests/middleware_test.rs`:
```rust
use dns_filter_core::middleware::metrics::MetricsMiddleware;
use dns_filter_plugin::Middleware;

// MetricsMiddleware counter test requires constructing a mock Request.
// For Phase 1, verify the counter API:
#[test]
fn metrics_counter_starts_at_zero() {
    let metrics = MetricsMiddleware::new();
    assert_eq!(metrics.total_queries(), 0);
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p dns-filter-core`
Expected: All tests pass

- [ ] **Step 3: Write placeholder for registry**

`crates/dns-filter-plugin/src/registry.rs`:
```rust
// PluginRegistry is deferred — main.rs builds the chain directly from config.
// This module is reserved for a future dynamic plugin system.
```

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test: add middleware unit tests"
```

---

## Chunk 5: Binary Entrypoint, Remote Config, & Shutdown

### Task 14: Wire Up main.rs

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Implement the full main.rs**

`src/main.rs`:
```rust
use anyhow::{Context, Result};
use dns_filter_core::config::{load_config, DnsFilterConfig, PluginKind};
use dns_filter_core::handler::DnsFilterHandler;
use dns_filter_core::middleware::logging::LoggingMiddleware;
use dns_filter_core::middleware::metrics::MetricsMiddleware;
use dns_filter_core::server::build_server;
use dns_filter_nacos::authority::NacosAuthority;
use dns_filter_nacos::watcher::NacosServiceWatcher;
use dns_filter_plugin::authority_chain::AuthorityChain;
use dns_filter_plugin::builtin::forward::ForwardAuthority;
use dns_filter_plugin::builtin::system_dns::SystemAuthority;
use dns_filter_plugin::Middleware;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dns_filter=info".into()),
        )
        .init();

    tracing::info!("dns-filter starting...");

    // Load config
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/dns-filter.toml".to_string());
    let config = load_config(&config_path)
        .context(format!("failed to load config from {}", config_path))?;

    // Build middleware chain
    let mut middlewares: Vec<Arc<dyn Middleware>> = Vec::new();
    if config.middleware.logging {
        middlewares.push(Arc::new(LoggingMiddleware));
    }
    if config.middleware.metrics {
        middlewares.push(Arc::new(MetricsMiddleware::new()));
    }

    // Build authority chain from plugins
    let authority_chain = build_authority_chain(&config).await?;

    // Create handler
    let handler = DnsFilterHandler::new(middlewares, Arc::new(authority_chain));

    // Build and start server
    let mut server = build_server(&config, handler).await?;

    tracing::info!("dns-filter is ready");

    // Wait for shutdown signal
    let shutdown_token = server.shutdown_token().clone();
    tokio::spawn(async move {
        let ctrl_c = signal::ctrl_c();
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT, shutting down..."),
            _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down..."),
        }
        shutdown_token.cancel();
    });

    server.block_until_done().await?;
    tracing::info!("dns-filter shut down gracefully");

    Ok(())
}

async fn build_authority_chain(config: &DnsFilterConfig) -> Result<AuthorityChain> {
    let mut handlers: Vec<Arc<dyn hickory_server::zone_handler::ZoneHandler>> = Vec::new();

    if config.plugins.is_empty() {
        // Default: forward to 8.8.8.8 and 1.1.1.1
        tracing::info!("no plugins configured, defaulting to forward");
        let forward = ForwardAuthority::new(
            vec!["8.8.8.8:53".to_string(), "1.1.1.1:53".to_string()],
            1024,
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await?;
        handlers.push(Arc::new(forward));
        return Ok(AuthorityChain::new(handlers));
    }

    for plugin in &config.plugins {
        if !plugin.enabled {
            continue;
        }

        match (&plugin.kind, &plugin.settings) {
            (PluginKind::Nacos, PluginSettings::Nacos {
                server_addr, namespace, group, dns_zone, ttl,
            }) => {
                let addr = server_addr.as_deref().unwrap_or("127.0.0.1:8848");
                let ns = namespace.as_deref().unwrap_or("public");
                let grp = group.as_deref().unwrap_or("DEFAULT_GROUP");
                let zone = dns_zone.as_deref().unwrap_or("nacos.local");

                let watcher = NacosServiceWatcher::new(addr, ns, grp).await?;
                // Fetch existing service list and subscribe to each
                watcher.subscribe_all().await;
                let cache = watcher.cache();

                let authority = NacosAuthority::new(cache, zone, *ttl);
                handlers.push(Arc::new(authority));
                tracing::info!("loaded nacos plugin: zone={}", zone);

                // Keep watcher alive (its subscriptions push updates)
                // In production, store watcher in a shared state holder
                std::mem::forget(watcher);
            }
            (PluginKind::SystemDns, PluginSettings::Resolver {
                cache_size, min_ttl, max_ttl, ..
            }) => {
                let authority = SystemAuthority::new(
                    *cache_size as u64,
                    Duration::from_secs(*min_ttl as u64),
                    Duration::from_secs(*max_ttl as u64),
                ).await?;
                handlers.push(Arc::new(authority));
                tracing::info!("loaded system_dns plugin");
            }
            (PluginKind::Forward, PluginSettings::Resolver {
                cache_size, min_ttl, max_ttl, upstream,
            }) => {
                let addrs = upstream
                    .clone()
                    .unwrap_or_else(|| vec!["8.8.8.8:53".to_string(), "1.1.1.1:53".to_string()]);
                let authority = ForwardAuthority::new(
                    addrs,
                    *cache_size as u64,
                    Duration::from_secs(*min_ttl as u64),
                    Duration::from_secs(*max_ttl as u64),
                ).await?;
                handlers.push(Arc::new(authority));
                tracing::info!("loaded forward plugin");
            }
            _ => {
                anyhow::bail!("mismatched plugin kind and settings");
            }
        }
    }

    Ok(AuthorityChain::new(handlers))
}
```

- [ ] **Step 2: Verify the full project compiles**

Run: `cargo check`
Expected: Compiles

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: wire up main.rs with config, middleware, and authority chain"
```

---

### Task 15: Nacos Config Watcher (Remote Hot-Reload)

**Files:**
- Create: `crates/dns-filter-nacos/src/config_watcher.rs`

- [ ] **Step 1: Implement NacosConfigWatcher**

`crates/dns-filter-nacos/src/config_watcher.rs`:
```rust
use arc_swap::ArcSwap;
use nacos_sdk::api::config::{ConfigChangeListener, ConfigResponse, ConfigService, ConfigServiceBuilder};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;

/// Watches Nacos config service for changes and triggers a callback.
pub struct NacosConfigWatcher {
    config_service: ConfigService,
    data_id: String,
    group: String,
}

struct ConfigReloadListener<F: Fn(String) + Send + Sync + 'static> {
    callback: F,
}

impl<F: Fn(String) + Send + Sync + 'static> ConfigChangeListener for ConfigReloadListener<F> {
    fn notify(&self, config_resp: ConfigResponse) {
        let content = config_resp.content().to_string();
        tracing::info!(
            data_id = config_resp.data_id(),
            "received config update from nacos"
        );
        (self.callback)(content);
    }
}

impl NacosConfigWatcher {
    pub async fn new(
        server_addr: &str,
        namespace: &str,
        group: &str,
        data_id: &str,
    ) -> anyhow::Result<Self> {
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name("dns-filter");

        let config_service = ConfigServiceBuilder::new(props).build()?;

        Ok(Self {
            config_service,
            data_id: data_id.to_string(),
            group: group.to_string(),
        })
    }

    /// Start listening for config changes. The callback receives the new TOML content.
    pub async fn start_watching<F>(&self, callback: F) -> anyhow::Result<()>
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        let listener = ConfigReloadListener { callback };

        self.config_service
            .add_listener(
                self.data_id.clone(),
                self.group.clone(),
                Arc::new(listener),
            )
            .await;

        tracing::info!(
            data_id = %self.data_id,
            group = %self.group,
            "watching nacos config for changes"
        );

        Ok(())
    }

    /// Get current config content.
    pub async fn get_current_config(&self) -> anyhow::Result<String> {
        let resp = self
            .config_service
            .get_config(self.data_id.clone(), self.group.clone())
            .await?;
        Ok(resp.content().to_string())
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p dns-filter-nacos`
Expected: Compiles

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: add NacosConfigWatcher for remote config hot-reload"
```

---

### Task 16: Full Build Verification & Cleanup

**Files:**
- All crate files

- [ ] **Step 1: Run full workspace build**

Run: `cargo build`
Expected: Builds successfully

- [ ] **Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: All tests pass

- [ ] **Step 3: Fix any compilation errors (CRITICAL STEP)**

This is the most important step. The plan contains code written against the documented hickory-dns 0.26.0-beta.1 API, but exact signatures may differ. Areas that commonly need adjustment:

**Import paths:**
- `hickory_server::zone_handler::ZoneHandler` — may be at `hickory_server::authority::ZoneHandler`
- `hickory_server::zone_handler::MessageResponseBuilder` — may be at `hickory_server::server::response_handler`
- `LookupControlFlow`, `AuthLookup`, `LookupOptions` — verify exact paths via `cargo doc`

**Constructor signatures:**
- `AuthLookup::answers(records.into(), None)` — the `answers()` constructor may take `LookupRecords` directly or require a different conversion. Try `AuthLookup::from(records)` or check `AuthLookup::new_with_records()` as alternatives.
- `Server::new(handler)` — may require `Server::builder(handler).build()` or similar

**Resolver builder:**
- `Resolver::builder_with_config(config, TokioRuntimeProvider::default())` — may be `Resolver::new(config, opts, provider)` or `TokioResolver::new(config, opts)`. Try the generic builder first, fall back to direct construction.
- `Resolver::builder_tokio()?` — may be `Resolver::from_system_conf(TokioRuntimeProvider::default())`
- `ResolverOpts` fields: `cache_size` may be `usize` not `u64`, `positive_min_ttl`/`positive_max_ttl` may have different names

**Response handler:**
- `ResponseHandler::send_response()` takes `MessageResponse` with complex generics. If the build fails on generic inference, try explicit type annotations or use `builder.error_msg()` for error responses.

**How to debug:** Run `cargo doc --open -p hickory-server` to browse the actual API locally.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: fix build issues and pass clippy"
```

---

## Chunk 6: Integration Tests & E2E

### Task 17: Integration Test — Authority Chain with Mock Data

**Files:**
- Create: `tests/integration_test.rs`

- [ ] **Step 1: Write integration test**

`tests/integration_test.rs`:
```rust
use dashmap::DashMap;
use dns_filter_nacos::authority::NacosAuthority;
use dns_filter_nacos::watcher::{CachedInstance, ServiceKey};
use dns_filter_plugin::authority_chain::AuthorityChain;
use dns_filter_plugin::builtin::forward::ForwardAuthority;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

fn setup_nacos_cache() -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "web-app".to_string(),
            group: "DEFAULT_GROUP".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "10.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }],
    );
    cache
}

#[tokio::test]
async fn chain_nacos_hit() {
    let cache = setup_nacos_cache();
    let nacos = Arc::new(NacosAuthority::new(cache, "nacos.local", 6));
    let forward = Arc::new(
        ForwardAuthority::new(
            vec!["8.8.8.8:53".to_string()],
            256,
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await
        .unwrap(),
    );

    let chain = AuthorityChain::new(vec![nacos, forward]);

    let name = LowerName::from(
        Name::from_str("web-app.DEFAULT_GROUP.public.nacos.local.").unwrap(),
    );
    let result = chain.resolve(&Name::from(name), RecordType::A, None).await;

    assert!(matches!(result, LookupControlFlow::Continue(Ok(_))));
}

#[tokio::test]
async fn chain_nacos_miss_falls_to_forward() {
    let cache = Arc::new(DashMap::new()); // empty cache
    let nacos = Arc::new(NacosAuthority::new(cache, "nacos.local", 6));
    let forward = Arc::new(
        ForwardAuthority::new(
            vec!["8.8.8.8:53".to_string()],
            256,
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await
        .unwrap(),
    );

    let chain = AuthorityChain::new(vec![nacos, forward]);

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = chain.resolve(&Name::from(name), RecordType::A, None).await;

    assert!(matches!(
        result,
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_))
    ));
}
```

- [ ] **Step 2: Run integration tests**

Run: `cargo test --test integration_test`
Expected: Both tests PASS

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test: add integration tests for authority chain"
```

---

### Task 18: Update .gitignore and README

**Files:**
- Modify: `.gitignore`
- Modify: `README.md`

- [ ] **Step 1: Update .gitignore**

Remove `Cargo.lock` from `.gitignore` since this is a binary project (not a library):

Replace the line:
```
Cargo.lock
```
with nothing (remove it).

- [ ] **Step 2: Add Cargo.lock to version control**

Run: `cargo generate-lockfile && git add Cargo.lock`

- [ ] **Step 3: Update README.md**

Add a "Quick Start" section and usage instructions reflecting the actual implemented architecture. Keep the existing content and append build/run instructions:

```markdown
## Quick Start

### Build

```bash
cargo build --release
```

### Run

```bash
# With default config
./target/release/dns-filter config/dns-filter.toml

# With debug logging
RUST_LOG=dns_filter=debug ./target/release/dns-filter config/dns-filter.toml
```

### Test

```bash
# Run all tests
cargo test --workspace

# Query the server (once running)
dig @127.0.0.1 -p 53 user-service.DEFAULT_GROUP.public.nacos.local A
```
```

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs: update README with build/run instructions"
```

---

## API Compatibility Note

This plan targets **hickory-dns 0.26.0-beta.1** which is a pre-release. The code in this plan is written against documented APIs but may require adjustments at compile time. **Task 16 is the designated step for resolving API mismatches.** Common patterns:
- Import paths may differ (use `cargo doc` to verify)
- Constructor signatures may vary
- Generic type inference may require explicit annotations
- nacos-sdk 0.6 may have slight API differences from 0.5.x documentation

When in doubt, run `cargo doc --open -p <crate>` to browse the actual API locally.

---

## Summary

| Task | Description | Commit |
|------|-------------|--------|
| 1 | Initialize cargo workspace | `feat: initialize cargo workspace` |
| 2 | Config parsing + tests + validation | `feat: add TOML config parsing with tests` |
| 3 | Plugin trait definitions | `feat: define Middleware and AuthorityPlugin traits` |
| 4 | AuthorityChain + test | `feat: add AuthorityChain` |
| 5 | ForwardAuthority + test | `feat: add ForwardAuthority` |
| 6 | SystemAuthority + test | `feat: add SystemAuthority` |
| 7 | Nacos DNS mapping + tests | `feat: add Nacos DNS name parsing and record mapping` |
| 8 | Nacos service watcher + subscribe_all | `feat: add NacosServiceWatcher` |
| 9 | NacosAuthority + tests | `feat: add NacosAuthority` |
| 10 | Logging + metrics middleware | `feat: add logging and metrics middleware` |
| 11 | DnsFilterHandler | `feat: add DnsFilterHandler` |
| 12 | Server setup (UDP/TCP/TLS/HTTPS) | `feat: add server setup with listeners` |
| 13 | Middleware + handler unit tests | `test: add middleware unit tests` |
| 14 | Wire up main.rs | `feat: wire up main.rs` |
| 15 | Nacos config watcher | `feat: add NacosConfigWatcher` |
| 16 | Full build verification (CRITICAL) | `chore: fix build issues` |
| 17 | Integration tests | `test: add integration tests` |
| 18 | README + gitignore | `docs: update README` |
