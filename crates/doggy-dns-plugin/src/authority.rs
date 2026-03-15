use anyhow::Result;
use hickory_server::zone_handler::ZoneHandler;
use std::sync::Arc;

/// Factory trait for creating a ZoneHandler from a plugin config section.
/// Each backend crate (nacos, native, forward) implements this trait.
pub trait AuthorityPlugin: Send + Sync {
    /// Create a ZoneHandler from config values.
    fn create_zone_handler(&self) -> Result<Arc<dyn ZoneHandler>>;
}
