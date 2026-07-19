# Integration Guide

For developers adding license checks to an app that Signet issues licenses for. Your app ships the public key and verifies licenses locally: no license server, no runtime dependency on Signet, no network call. This guide covers verifying a license blob and reading its claims. For issuing licenses and running the shop, see the [operator guide](./operator-guide.md).

## How it fits together

A license is a small base64 blob carrying signed claims (who it's for, which product line, which features, when it expires). Each product line has its own Ed25519 keypair. You bake the public half into your app; verification is a local signature check against that key.

The key is the real gate. Because each product signs with a distinct key, a license for one product will not verify against another product's key. The `product` claim inside the license is defense in depth, not the boundary, so your app only ever holds and checks its own product's key.

## Adding signetlib

Depend on `signetlib` and `ed25519-dalek` v2 (you construct the verifying key with the latter):

```toml
[dependencies]
signetlib = { git = "https://github.com/franzos/signet" }  # or a path / published version
ed25519-dalek = "2"
```

If you'd rather not take the dependency, the wire format is small enough to reimplement (see the end of this guide), but `signetlib` is the supported path.

## Getting the public key into your app

`signet-issuer keygen` writes the public key as a raw 32-byte file. The simplest thing is to embed it at compile time:

```rust
use ed25519_dalek::VerifyingKey;

// keys/acme/public.bin from `signet-issuer keygen --product acme`
const PUBLIC_KEY: &[u8; 32] = include_bytes!("../keys/acme/public.bin");

fn verifying_key() -> VerifyingKey {
    VerifyingKey::from_bytes(PUBLIC_KEY).expect("valid public key")
}
```

If you'd rather load it from a file at runtime, `signetlib::codec::load_verifying_key(path)` reads the same 32-byte format.

## Verifying a blob

`decode_and_verify` checks the signature and returns the claims:

```rust
use signetlib::claims::Claims;
use signetlib::codec::{decode_and_verify, DecodeError};

fn verify(blob: &str) -> Result<Claims, DecodeError> {
    decode_and_verify(blob.trim(), &verifying_key())
}
```

`DecodeError` has two cases, and both mean "don't trust this license": `Malformed(..)` (not a valid blob: bad base64, wrong format, corrupt) and `BadSignature` (well-formed, but no key verified it). Trim the input; whitespace around a pasted blob is common.

The claims you get back:

```rust
pub struct Claims {
    pub v: u8,                       // schema version
    pub license_id: String,          // correlates with the issuer's ledger
    pub customer: String,
    pub email: String,
    pub tier: String,                // free-form marketing label
    pub product: String,             // product line id
    pub issued_at: i64,              // Unix seconds
    pub expires_at: Option<i64>,     // Unix seconds; None = lifetime
    pub features: Vec<String>,       // the flags to gate on
    pub max_orgs: Option<u32>,       // None = unlimited
    pub max_seats: Option<u32>,      // None = unlimited
    pub note: String,                // ledger-only
}
```

## Checking expiry and features

`signetlib` verifies the signature; deciding what a valid license grants is up to you. `Claims` is plain data with no helper methods, so check the fields directly:

```rust
fn now_unix() -> i64 { /* your clock, in seconds */ }

let claims = verify(blob)?;

// Expiry: None means lifetime.
let expired = claims.expires_at.is_some_and(|exp| now_unix() >= exp);

// Gate on features and limits.
let has_orgs = claims.features.iter().any(|f| f == "orgs");
let seat_cap = claims.max_seats;  // None = unlimited
```

Gate on `features` (and `max_orgs` / `max_seats`), not on `tier`: the tier string is a label, while the features are what the issuer actually granted.

## Key rotation

The two-key setup (an offline root key, a server-side web key the shop signs with) lets you rotate the web key without reissuing every license. To stay verifiable across a rotation, embed both public keys and accept a match from either:

```rust
use signetlib::codec::decode_and_verify_any;

const ROOT_PUB: &[u8; 32] = include_bytes!("../keys/acme/public.bin");
const WEB_PUB:  &[u8; 32] = include_bytes!("../keys/acme/web-public.bin");

let keys = [
    VerifyingKey::from_bytes(ROOT_PUB).expect("valid key"),
    VerifyingKey::from_bytes(WEB_PUB).expect("valid key"),
];
let claims = decode_and_verify_any(blob.trim(), &keys)?;
```

Note there's no revocation: a signed license is valid until it expires. To cut one off early you rotate the signing key and reissue, so plan expiries accordingly.

## Wire format (if you're not using signetlib)

You don't need this if you use `signetlib`, but for a reimplementation in another language: a blob is `base64_standard( "OPLB" + version_byte(1) + CBOR(envelope) )`, where the envelope is `{ c: <CBOR of the claims>, s: <64-byte Ed25519 signature> }`. The signature is over the exact CBOR claim bytes as they appear in the blob (so map ordering can't break verification), and unknown claim fields are ignored for forward compatibility. Verify the signature first, then decode the claims.
