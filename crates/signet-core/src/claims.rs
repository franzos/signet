//! Shared license-claim shape. Mirrored exactly in your application's
//! license verifier: keep the two in sync.
//!
//! Wire format on disk / in the activate textarea:
//!
//! ```text
//! base64( BLOB_MAGIC || version_byte || cbor(SignedBlob) )
//! ```
//!
//! `SignedBlob.claims_cbor` is the canonical CBOR encoding of [`Claims`];
//! `SignedBlob.signature` is an Ed25519 signature over `claims_cbor`. The
//! magic + version prefix makes future format changes detectable without
//! gambling on CBOR self-description.
//!
//! All timestamps are Unix seconds (i64) so the on-wire shape stays the
//! same across timezones and chrono / time-crate choices.

use serde::{Deserialize, Serialize};

/// 4-byte magic that prefixes every license blob. Lets a quick byte-level
/// check reject obvious garbage before we spend CBOR-decode work on it.
pub const BLOB_MAGIC: &[u8; 4] = b"OPLB";

/// Wire-format version. Bump if [`Claims`] gains a non-backwards-compatible
/// field; older apps reject unknown versions.
pub const BLOB_VERSION: u8 = 1;

/// CBOR-serialised payload inside the signed blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedBlob {
    #[serde(rename = "c", with = "serde_bytes")]
    pub claims_cbor: Vec<u8>,
    #[serde(rename = "s", with = "serde_bytes")]
    pub signature: Vec<u8>,
}

/// The actual license claims. Encoded as CBOR and signed; the app
/// re-encodes on read and compares the signature against the bytes it
/// observed (carried as `claims_cbor` inside the envelope), not against a
/// re-serialised copy — so map-ordering differences across implementations
/// can't break verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Schema version of the *inner* claim shape. Distinct from
    /// [`BLOB_VERSION`] so we can evolve claim fields without changing the
    /// outer wire envelope.
    pub v: u8,
    /// Stable identifier for ledger correlation + support debugging. UUID
    /// v4 hex (32 chars). Not security-relevant.
    pub license_id: String,
    pub customer: String,
    pub email: String,
    /// Marketed tier name: "light" | "pro" | "enterprise". Free-form so
    /// new SKUs don't require an issuer-side enum update.
    pub tier: String,
    /// Product this license is valid for: "acme" | "globex" | future.
    /// Per-product signing keys are the real gate; this is defense-in-depth
    /// and drives clearer "wrong product" errors in the app.
    #[serde(default)]
    pub product: String,
    /// Unix seconds.
    pub issued_at: i64,
    /// Unix seconds. `None` = lifetime license (no expiry).
    pub expires_at: Option<i64>,
    /// Feature flags this license unlocks. See the app's Feature enum for
    /// the names the application recognises; unknown features are ignored
    /// at gate time.
    #[serde(default)]
    pub features: Vec<String>,
    /// Per-quota limits. `None` = unlimited.
    #[serde(default)]
    pub max_orgs: Option<u32>,
    #[serde(default)]
    pub max_seats: Option<u32>,
    /// Free-form note recorded in the ledger; not surfaced in the app UI.
    #[serde(default)]
    pub note: String,
}
