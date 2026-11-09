//! The wire-level query builder now lives in `doggy-dns-testutil`, shared with
//! the sibling crates; this module keeps the `mod common;` import every
//! `doggy-dns-core` test target and the handler benchmark already uses.

pub use doggy_dns_testutil::dns_request;
