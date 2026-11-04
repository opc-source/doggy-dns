//! Library surface of the `doggy-dns` package.
//!
//! The binary owns startup (`src/main.rs`) and the XDP wiring
//! (`src/xdp/`); the reload pipeline is exposed here so `tests/` can drive it
//! directly — startup and reload share [`reload::build_chain`], and a
//! coordinator tested through the public API is tested through the same code
//! path the triggers call.

pub mod reload;
