//! Encode / decode the on-wire license blob, and run signature
//! verification. Your application pulls a near-identical decoder into its
//! own license verifier; keep them in sync.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SIGNATURE_LENGTH};

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
    /// Parses fine, but the signature doesn't verify against the configured
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

/// Decode a base64-encoded license blob and verify its signature against
/// `verifying_key`. Returns the parsed claims on success.
pub fn decode_and_verify(b64: &str, verifying_key: &VerifyingKey) -> Result<Claims, DecodeError> {
    let bytes = B64
        .decode(b64.trim())
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

    verifying_key
        .verify(&blob.claims_cbor, &signature)
        .map_err(|_| DecodeError::BadSignature)?;

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
