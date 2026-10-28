//! Lightweight doggy-dns configuration types.
//!
//! This crate holds every TOML config struct and validation rule, with no
//! transitive dependency on hickory, tokio, rustls, or the DNS runtime itself.
//! Consumers (observability, xdp, mcp, tests) can depend on it alone.

pub mod control;
pub mod env;
pub mod load;
pub mod middleware;
pub mod net;
pub mod observability;
pub mod plugin;
pub mod remote;
pub mod server;
pub mod validate;
pub mod xdp;

use serde::Deserialize;

pub use control::{CONTROL_ENDPOINTS, ControlConfig};
pub use load::load_config;
pub use middleware::MiddlewareConfig;
pub use observability::{
    BasicAuthConfig, LogsConfig, MetricsConfig, MetricsTemporality, ObservabilityBackend,
    ObservabilityConfig, ObservabilityProtocol, ProfilesConfig, TracesConfig,
};
pub use plugin::{PluginConfig, PluginKind};
pub use remote::RemoteConfig;
pub use server::{HttpsConfig, ServerConfig, TlsConfig};
pub use validate::validate_config;
pub use xdp::XdpConfig;

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
    #[serde(default)]
    pub control: ControlConfig,
}
