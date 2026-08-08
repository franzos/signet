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

    /// Hand-build the wire envelope from raw parts, bypassing `encode`'s
    /// signing so tests can inject malformed pieces.
    fn envelope(claims_cbor: Vec<u8>, signature: Vec<u8>) -> String {
        let blob = SignedBlob {
            claims_cbor,
            signature,
        };
        let mut env = Vec::new();
        env.extend_from_slice(BLOB_MAGIC);
        env.push(BLOB_VERSION);
        ciborium::into_writer(&blob, &mut env).unwrap();
        B64.encode(env)
    }

    #[test]
    fn tampered_signature_is_bad_signature() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let mut bytes = B64
            .decode(encode(&test_claims(), &signer).unwrap())
            .unwrap();
        *bytes.last_mut().unwrap() ^= 0x01;
        assert!(matches!(
            decode_and_verify(&B64.encode(bytes), &signer.verifying_key()),
            Err(DecodeError::BadSignature)
        ));
    }

    #[test]
    fn wrong_magic_is_malformed() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let mut bytes = B64
            .decode(encode(&test_claims(), &signer).unwrap())
            .unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            decode_and_verify(&B64.encode(bytes), &signer.verifying_key()),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_version_is_malformed() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let mut bytes = B64
            .decode(encode(&test_claims(), &signer).unwrap())
            .unwrap();
        bytes[BLOB_MAGIC.len()] = 99;
        assert!(matches!(
            decode_and_verify(&B64.encode(bytes), &signer.verifying_key()),
            Err(DecodeError::Malformed(m)) if m.contains("version")
        ));
    }

    #[test]
    fn truncated_blob_is_malformed() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        assert!(matches!(
            decode_and_verify(&B64.encode(b"OPL"), &signer.verifying_key()),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn non_base64_is_malformed() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        assert!(matches!(
            decode_and_verify("!!! not base64 !!!", &signer.verifying_key()),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn wrong_signature_length_is_malformed() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let mut claims_cbor = Vec::new();
        ciborium::into_writer(&test_claims(), &mut claims_cbor).unwrap();
        let b64 = envelope(claims_cbor, vec![0u8; 10]);
        assert!(matches!(
            decode_and_verify(&b64, &signer.verifying_key()),
            Err(DecodeError::Malformed(m)) if m.contains("signature length")
        ));
    }

    #[test]
    fn unknown_claim_field_is_ignored() {
        // A newer issuer adds a field an older app doesn't know about.
        #[derive(serde::Serialize)]
        struct ExtraClaims {
            v: u8,
            license_id: String,
            customer: String,
            email: String,
            tier: String,
            product: String,
            issued_at: i64,
            expires_at: Option<i64>,
            features: Vec<String>,
            max_orgs: Option<u32>,
            max_seats: Option<u32>,
            note: String,
            #[serde(rename = "x")]
            extra: u8,
        }
        let c = test_claims();
        let extended = ExtraClaims {
            v: c.v,
            license_id: c.license_id.clone(),
            customer: c.customer.clone(),
            email: c.email,
            tier: c.tier,
            product: c.product,
            issued_at: c.issued_at,
            expires_at: c.expires_at,
            features: c.features,
            max_orgs: c.max_orgs,
            max_seats: c.max_seats,
            note: c.note,
            extra: 7,
        };

        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let mut claims_cbor = Vec::new();
        ciborium::into_writer(&extended, &mut claims_cbor).unwrap();
        let signature = signer.sign(&claims_cbor).to_bytes().to_vec();
        let b64 = envelope(claims_cbor, signature);

        let claims = decode_and_verify(&b64, &signer.verifying_key()).unwrap();
        assert_eq!(claims.license_id, c.license_id);
        assert_eq!(claims.customer, c.customer);
    }

    #[test]
    fn full_roundtrip_all_fields() {
        let signer = SigningKey::from_bytes(&[9u8; 32]);
        let claims = Claims {
            v: CLAIMS_VERSION,
            license_id: "lic-9".into(),
            customer: "Globex".into(),
            email: "ops@globex.example".into(),
            tier: "enterprise".into(),
            product: "globex".into(),
            issued_at: 1_700_000_000,
            expires_at: Some(1_900_000_000),
            features: vec!["sso".into(), "audit".into(), "export".into()],
            max_orgs: Some(5),
            max_seats: Some(250),
            note: "renewal, net-30".into(),
        };
        let got =
            decode_and_verify(&encode(&claims, &signer).unwrap(), &signer.verifying_key()).unwrap();
        assert_eq!(got.v, claims.v);
        assert_eq!(got.license_id, claims.license_id);
        assert_eq!(got.customer, claims.customer);
        assert_eq!(got.email, claims.email);
        assert_eq!(got.tier, claims.tier);
        assert_eq!(got.product, claims.product);
        assert_eq!(got.issued_at, claims.issued_at);
        assert_eq!(got.expires_at, claims.expires_at);
        assert_eq!(got.features, claims.features);
        assert_eq!(got.max_orgs, claims.max_orgs);
        assert_eq!(got.max_seats, claims.max_seats);
        assert_eq!(got.note, claims.note);

        let lifetime = Claims {
            expires_at: None,
            ..claims
        };
        let got = decode_and_verify(
            &encode(&lifetime, &signer).unwrap(),
            &signer.verifying_key(),
        )
        .unwrap();
        assert_eq!(got.expires_at, None);
        assert_eq!(got.features, lifetime.features);
    }
}
