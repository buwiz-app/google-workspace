//! The mini app protocol pieces the plugin signs and checks with: JCS
//! (RFC 8785) and keys. Copied from the Bot's `buwiz-protocol` crate (formerly `rox-v1`)
//! (buwiz-bot/buwiz-bot `broker/buwiz-protocol`), so the bytes match the Bot's.

pub mod jcs;
pub mod keys;
