use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;

use crate::claims::{Claims, BLOB_VERSION};
use crate::codec;

/// Everything a caller supplies to mint a license. `now_unix` and the random
/// `license_id` are stamped inside `issue()` so the CLI and the shop cannot
/// drift on how those are derived.
pub struct IssueParams {
    pub product: String,
    pub customer: String,
    pub email: String,
    pub tier: String,
    pub expires_at: Option<i64>,
    pub features: Vec<String>,
    pub max_orgs: Option<u32>,
    pub max_seats: Option<u32>,
    pub note: String,
}

pub struct Issued {
    pub blob: String,
    pub claims: Claims,
}

pub fn issue(
    params: IssueParams,
    now_unix: i64,
    signing_key: &SigningKey,
) -> Result<Issued, codec::Error> {
    let claims = Claims {
        v: BLOB_VERSION,
        license_id: random_license_id(),
        customer: params.customer,
        email: params.email,
        tier: params.tier,
        product: params.product,
        issued_at: now_unix,
        expires_at: params.expires_at,
        features: params.features,
        max_orgs: params.max_orgs,
        max_seats: params.max_seats,
        note: params.note,
    };
    let blob = codec::encode(&claims, signing_key)?;
    Ok(Issued { blob, claims })
}

fn random_license_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn issue_roundtrips_and_stamps_metadata() {
        let key = test_key();
        let params = IssueParams {
            product: "acme".into(),
            customer: "Acme GmbH".into(),
            email: "admin@acme.example".into(),
            tier: "business".into(),
            expires_at: Some(1_800_000_000),
            features: vec!["orgs".into(), "saml".into()],
            max_orgs: Some(50),
            max_seats: None,
            note: "web".into(),
        };
        let issued = issue(params, 1_700_000_000, &key).unwrap();

        // Wire format verifies against the matching public key.
        let verified = crate::codec::decode_and_verify(&issued.blob, &key.verifying_key()).unwrap();
        assert_eq!(verified.product, "acme");
        assert_eq!(verified.issued_at, 1_700_000_000);
        assert_eq!(verified.expires_at, Some(1_800_000_000));
        assert_eq!(verified.features, vec!["orgs", "saml"]);
        assert_eq!(verified.max_orgs, Some(50));
        assert_eq!(verified.v, crate::claims::BLOB_VERSION);
        assert_eq!(issued.claims.license_id.len(), 32); // hex of 16 bytes
    }
}
