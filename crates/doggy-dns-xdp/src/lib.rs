pub mod cache;
pub mod interface;
pub mod middleware;
#[cfg(target_os = "linux")]
pub mod program;
pub mod types;

#[cfg(target_os = "linux")]
pub use middleware::XdpCacheMiddleware;
#[cfg(target_os = "linux")]
pub use program::XdpManager;
