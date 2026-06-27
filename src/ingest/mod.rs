//! The push path: a local unix-socket server receiving sanitized statusline and
//! hook events forwarded by the chained shell scripts (design §4.1).

pub mod events;
pub mod socket;
