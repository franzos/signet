//! Encode / decode the on-wire license blob, and run signature
//! verification against one or more trusted public keys. Your application
//! pulls a near-identical decoder into its own license verifier; keep them
//! in sync.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey, SIGNATURE_LENGTH};

use crate::claims::{Claims, SignedBlob, BLOB_MAGIC, BLOB_VERSION};

/// Failure modes for encoding, issuing, and key loading. Decoding has its
/// own [`DecodeError`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("read {}", path.display())]
    ReadKey {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("key at {} is {len} bytes, expected 32", path.display())]
    KeyLength {
        path: std::path::PathBuf,
        len: usize,
    },
    #[error("invalid verifying key")]
    VerifyingKey(#[source] ed25519_dalek::SignatureError),
    #[error("encode claims")]
    EncodeClaims(#[source] ciborium::ser::Error<std::io::Error>),
    #[error("encode envelope")]
    EncodeEnvelope(#[source] ciborium::ser::Error<std::io::Error>),
}

/// Encode a signed license into the wire blob. Signs the canonical CBOR
/// encoding of `claims` and wraps it in the envelope.
pub fn encode(claims: &Claims, signing_key: &SigningKey) -> Result<String, Error> {
    let mut claims_cbor = Vec::new();
    ciborium::into_writer(claims, &mut claims_cbor).map_err(Error::EncodeClaims)?;
    let signature = signing_key.sign(&claims_cbor);
    let blob = SignedBlob {
        claims_cbor,
        signature: signature.to_bytes().to_vec(),
    };

    let mut envelope = Vec::with_capacity(64);
    envelope.extend_from_slice(BLOB_MAGIC);
    envelope.push(BLOB_VERSION);
    ciborium::into_writer(&blob, &mut envelope).map_err(Error::EncodeEnvelope)?;

    Ok(B64.encode(envelope))
}

/// Outcome of [`decode_and_verify`]. Splits "looks like one of ours but bad
/// signature" from "doesn't even parse" so the app can surface a useful
/// error to the operator.
#[derive(Debug)]
pub enum DecodeError {
    /// Couldn't base64-decode, doesn't carry the magic, unknown version, or
    /// CBOR parse failure.
    Malformed(String),
    /// Parses fine, but the signature doesn't verify against any configured
    /// public key. Either tampered or signed with the wrong key.
    BadSignature,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Malformed(s) => write!(f, "malformed license blob: {s}"),
            DecodeError::BadSignature => write!(f, "license signature did not verify"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Upper bound on accepted input, enforced before any decode work.
/// A legitimate blob is well under 4 KB.
const MAX_BLOB_B64_LEN: usize = 16 * 1024;

/// Decode a base64-encoded license blob and verify its signature against
/// `verifying_key`. Returns the parsed claims on success.
pub fn decode_and_verify(b64: &str, verifying_key: &VerifyingKey) -> Result<Claims, DecodeError> {
    decode_and_verify_any(b64, std::slice::from_ref(verifying_key))
}

/// Like [`decode_and_verify`], but accepts the blob if its signature
/// verifies against *any* of `verifying_keys`. Lets an app embed both the
/// offline root public key and the shop's revocable web public key: rotating
/// the web key after a shop compromise doesn't invalidate root-signed
/// licenses. An empty slice yields [`DecodeError::BadSignature`].
pub fn decode_and_verify_any(
    b64: &str,
    verifying_keys: &[VerifyingKey],
) -> Result<Claims, DecodeError> {
    let b64 = b64.trim();
    if b64.len() > MAX_BLOB_B64_LEN {
        return Err(DecodeError::Malformed("blob too large".into()));
    }
    let bytes = B64
        .decode(b64)
        .map_err(|e| DecodeError::Malformed(format!("base64: {e}")))?;

    if bytes.len() < BLOB_MAGIC.len() + 1 {
        return Err(DecodeError::Malformed("blob too short".into()));
    }
    if &bytes[..BLOB_MAGIC.len()] != BLOB_MAGIC {
        return Err(DecodeError::Malformed("magic mismatch".into()));
    }
    let version = bytes[BLOB_MAGIC.len()];
    if version != BLOB_VERSION {
        return Err(DecodeError::Malformed(format!(
            "unsupported version {version}, expected {BLOB_VERSION}"
        )));
    }

    let cbor_start = BLOB_MAGIC.len() + 1;
    let blob: SignedBlob = ciborium::from_reader(&bytes[cbor_start..])
        .map_err(|e| DecodeError::Malformed(format!("envelope cbor: {e}")))?;

    if blob.signature.len() != SIGNATURE_LENGTH {
        return Err(DecodeError::Malformed(format!(
            "signature length {}, expected {SIGNATURE_LENGTH}",
            blob.signature.len()
        )));
    }
    let sig_bytes: [u8; SIGNATURE_LENGTH] = blob
        .signature
        .as_slice()
        .try_into()
        .expect("checked length above");
    let signature = Signature::from_bytes(&sig_bytes);

    if !verifying_keys
        .iter()
        .any(|key| key.verify_strict(&blob.claims_cbor, &signature).is_ok())
    {
        return Err(DecodeError::BadSignature);
    }

    let claims: Claims = ciborium::from_reader(blob.claims_cbor.as_slice())
        .map_err(|e| DecodeError::Malformed(format!("claims cbor: {e}")))?;

    Ok(claims)
}

/// Convenience for the CLI: load a raw 32-byte verifying key from disk.
pub fn load_verifying_key(path: &std::path::Path) -> Result<VerifyingKey, Error> {
    let arr = read_key_bytes(path)?;
    VerifyingKey::from_bytes(&arr).map_err(Error::VerifyingKey)
}

/// Convenience for the CLI: load a raw 32-byte signing key from disk.
pub fn load_signing_key(path: &std::path::Path) -> Result<SigningKey, Error> {
    Ok(SigningKey::from_bytes(&read_key_bytes(path)?))
}

fn read_key_bytes(path: &std::path::Path) -> Result<[u8; 32], Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::ReadKey {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.as_slice().try_into().map_err(|_| Error::KeyLength {
        path: path.to_path_buf(),
        len: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::CLAIMS_VERSION;

    fn test_claims() -> Claims {
        Claims {
            v: CLAIMS_VERSION,
            license_id: "abc123".into(),
            customer: "Acme GmbH".into(),
            email: "admin@acme.example".into(),
            tier: "business".into(),
            product: "acme".into(),
            issued_at: 1_700_000_000,
            expires_at: None,
            features: vec![],
            max_orgs: None,
            max_seats: None,
            note: String::new(),
        }
    }

    #[test]
    fn verify_any_accepts_second_key() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let other = SigningKey::from_bytes(&[1u8; 32]);
        let blob = encode(&test_claims(), &signer).unwrap();

        let keys = [other.verifying_key(), signer.verifying_key()];
        let claims = decode_and_verify_any(&blob, &keys).unwrap();
        assert_eq!(claims.customer, "Acme GmbH");
    }

    #[test]
    fn verify_any_rejects_when_no_key_matches() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let other = SigningKey::from_bytes(&[1u8; 32]);
        let blob = encode(&test_claims(), &signer).unwrap();

        let keys = [other.verifying_key()];
        assert!(matches!(
            decode_and_verify_any(&blob, &keys),
            Err(DecodeError::BadSignature)
        ));
        assert!(matches!(
            decode_and_verify_any(&blob, &[]),
            Err(DecodeError::BadSignature)
        ));
    }

    #[test]
    fn oversized_input_rejected_before_decoding() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let huge = "A".repeat(MAX_BLOB_B64_LEN + 1);
        assert!(matches!(
            decode_and_verify(&huge, &key.verifying_key()),
            Err(DecodeError::Malformed(_))
        ));
    }
}
