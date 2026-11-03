//! Library half of the `doggy-dns` package.
//!
//! Only what the binary needs to share with the test suite lives here:
//! the config-reload orchestrator and the authority-chain builder. The
//! start-up sequence itself stays in `main.rs`.

pub mod reload;
