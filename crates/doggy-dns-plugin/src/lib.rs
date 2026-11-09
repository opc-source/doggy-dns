pub mod authority;
pub mod authority_chain;
pub mod builtin;
pub mod naming;
pub mod plugin;
pub mod registry;
pub mod zone_handler;

pub use authority::AuthorityPlugin;
pub use plugin::{Middleware, MiddlewareAction, RequestOutcome};

pub const APP_NAME: &str = "doggy-dns";
