//! Reversible install actions: launchd agent, chained hooks, and the statusline
//! wrapper. Every action chains (never overwrites) the user's config (ADR-4).

pub mod hooks;
pub mod launchd;
pub mod paths;
pub mod statusline;
