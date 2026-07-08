//! Offline per-product license signing: claims shape, wire codec, and the
//! shared `issue()` entry point used by both the CLI and the web shop.

pub mod claims;
pub mod codec;
mod issue;

pub use issue::{issue, IssueParams, Issued};
