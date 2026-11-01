//! The mini app protocol pieces the plugin signs and checks with: JCS
//! (RFC 8785) and keys. Copied from the Bot's `rox-v1` crate
//! (buwiz-bot/buwiz-bot `broker/rox-v1`), so the bytes match the Bot's.

pub mod jcs;
pub mod keys;
