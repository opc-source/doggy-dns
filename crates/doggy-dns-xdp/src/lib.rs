pub mod cache;
pub mod interface;
pub mod kernel_log;
pub mod middleware;
#[cfg(target_os = "linux")]
mod pod_impls;
#[cfg(target_os = "linux")]
pub mod program;
pub mod stats;
pub mod types;

#[cfg(target_os = "linux")]
pub use middleware::XdpCacheMiddleware;
#[cfg(target_os = "linux")]
pub use program::XdpManager;

// Component benchmarks in this crate are re-measured against main
// on every PR by the xdp-component-bench job in .github/workflows/xdp-ci.yml.
