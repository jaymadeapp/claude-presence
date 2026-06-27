//! Reads of the internal `~/.claude` layout, isolated behind versioned adapters
//! (ADR-5). All schema reads go through `schema`; nothing here trusts argv.

pub mod activity;
pub mod pricing;
pub mod schema;
pub mod sessions;
pub mod transcript;
