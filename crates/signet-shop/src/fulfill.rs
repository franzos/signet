use anyhow::{anyhow, Result};
use chrono::Utc;

use crate::cache::NegEntry;
use crate::catalog::Sku;
use crate::db::{self, StoredLicense};
use crate::mail::PurchaseNotice;
use crate::payments::{self, SessionInfo};
use crate::state::AppState;

pub enum FulfillOutcome {
    Ready(StoredLicense),
    Pending,
    /// Stripe has no such session: unknown or long-expired id.
    LookupFailed,
}

pub async fn fulfill(state: &AppState, session_id: &str) -> Result<FulfillOutcome> {
    // Fast path: already fulfilled, no need to hit Stripe. Stays FIRST so a
    // webhook-completed fulfillment always wins over a stale neg-cache entry.
    if let Some(existing) = db::get_by_session(&state.db, session_id).await? {
        return Ok(FulfillOutcome::Ready(existing));
    }
    // Recently checked and still not fulfillable: skip the Stripe round-trip.
    match state.neg_cache.get(session_id) {
        Some(NegEntry::Unpaid) => return Ok(FulfillOutcome::Pending),
        Some(NegEntry::NotFound) => return Ok(FulfillOutcome::LookupFailed),
        None => {}
    }
    // A definitive not-found is cached so a flood of the same id costs one
    // call; transient Stripe errors are not, so recovery is immediate.
    let Some(info) = payments::retrieve_session(&state.stripe, session_id).await? else {
        state
            .neg_cache
            .insert(session_id.to_string(), NegEntry::NotFound);
        return Ok(FulfillOutcome::LookupFailed);
    };
    let outcome = fulfill_paid_session(state, &info).await?;
    if matches!(outcome, FulfillOutcome::Pending) {
        state
            .neg_cache
            .insert(session_id.to_string(), NegEntry::Unpaid);
    }
    Ok(outcome)
}

pub async fn fulfill_paid_session(
    state: &AppState,
    session: &SessionInfo,
) -> Result<FulfillOutcome> {
    if !session.paid {
        return Ok(FulfillOutcome::Pending);
    }
    if let Some(existing) = db::get_by_session(&state.db, &session.id).await? {
        return Ok(FulfillOutcome::Ready(existing));
    }

    // Do not put session.id into anyhow messages: they reach `tracing`, and the
    // id is the bearer secret for /success. Keep it out of logs.
    let sku_id = session
        .sku
        .as_deref()
        .ok_or_else(|| anyhow!("checkout session missing sku metadata"))?;
    let sku: &Sku = state
        .catalog
        .by_id(sku_id)
        .ok_or_else(|| anyhow!("unknown sku in session metadata"))?;
    // Trust Stripe's signature, but not the session's economics: a session that
    // paid less, in another currency, or in the wrong mode never mints. Pending
    // (not an error) keeps the webhook 2xx so Stripe does not retry forever.
    let expect_livemode = state.cfg.stripe_api_key.starts_with("sk_live");
    if let Some(reason) = payment_mismatch(session, sku, expect_livemode) {
        tracing::warn!(
            sku = %sku.id,
            "payment verification failed, not issuing: {reason}"
        );
        return Ok(FulfillOutcome::Pending);
    }
    let signing = state
        .signing
        .get(sku.category.as_str())
        .ok_or_else(|| anyhow!("no signing key for category {}", sku.category))?;

    let email = session.email.clone().unwrap_or_default();
    // The license identity is the company (checkout custom field), falling back
    // to the billing name if it is somehow absent.
    let customer = session
        .company
        .clone()
        .or_else(|| session.name.clone())
        .unwrap_or_default();

    let now = Utc::now().timestamp();
    let params = sku.to_issue_params(
        customer.clone(),
        email.clone(),
        now,
        format!("web:{}", session.id),
    );
    let issued = signetlib::issue(params, now, signing)?;
    let expires_at = issued.claims.expires_at;

    let rec = StoredLicense {
        stripe_session_id: session.id.clone(),
        license_id: issued.claims.license_id.clone(),
        product: sku.category.to_string(),
        sku: sku.id.to_string(),
        customer,
        email,
        blob: issued.blob,
        issued_at: now,
    };
    let (stored, inserted) = db::insert_or_get(&state.db, &rec).await?;
    // Send the purchase emails once, off the request path, only for the fresh
    // insert. A mail failure is logged, never surfaced: it must not make the
    // webhook 500 (which would have Stripe retry) nor block the /success page.
    if inserted {
        if let Some(mail) = state.mail.clone() {
            let notice = PurchaseNotice {
                buyer_email: stored.email.clone(),
                customer: stored.customer.clone(),
                product_display: sku.display_name.clone(),
                license_id: stored.license_id.clone(),
                license_blob: stored.blob.clone(),
                expires_at,
                price_label: price_label(sku),
            };
            tokio::spawn(async move { mail.notify_purchase(&notice).await });
        }
    }
    Ok(FulfillOutcome::Ready(stored))
}

/// Cross-check the session's economics against the catalog SKU. Returns the
/// failed check, or `None` when the session is safe to fulfill.
fn payment_mismatch(session: &SessionInfo, sku: &Sku, expect_livemode: bool) -> Option<String> {
    if session.livemode != expect_livemode {
        return Some(format!(
            "livemode is {} but the configured key expects {}",
            session.livemode, expect_livemode
        ));
    }
    match session.amount_total {
        Some(a) if a >= sku.amount_cents => {}
        other => {
            return Some(format!(
                "amount_total {other:?} is below the sku price {}",
                sku.amount_cents
            ));
        }
    }
    let currency_ok = session.currency.as_ref().is_some_and(|c| {
        c.to_string()
            .eq_ignore_ascii_case(&sku.currency.to_string())
    });
    if !currency_ok {
        return Some(format!(
            "currency {:?} does not match the sku currency {}",
            session.currency.as_ref().map(ToString::to_string),
            sku.currency
        ));
    }
    None
}

/// A human price for the operator notice: the catalog label when set, otherwise
/// derived from the SKU's amount and currency.
fn price_label(sku: &Sku) -> String {
    if sku.price_label.is_empty() {
        format!("{} {:.2}", sku.currency, sku.amount_cents as f64 / 100.0)
    } else {
        sku.price_label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_catalog() -> crate::catalog::Catalog {
        crate::catalog::parse(
            r#"
[[category]]
id = "acme"
name = "Acme"

[[category]]
id = "globex"
name = "Globex"

[[sku]]
id = "acme-business-annual"
category = "acme"
display_name = "Acme Business (annual)"
amount_cents = 49900
currency = "eur"
tier = "business"
features = ["orgs", "saml", "linux_auth"]
max_orgs = 50
max_seats = 200
term = "365d"

[[sku]]
id = "globex-pro-annual"
category = "globex"
display_name = "Globex Pro (annual)"
amount_cents = 29900
currency = "eur"
tier = "pro"
features = ["observability"]
term = "365d"
"#,
        )
        .unwrap()
    }

    async fn test_state() -> AppState {
        crate::ensure_crypto_provider();
        let cfg = crate::config::AppConfig {
            stripe_api_key: "sk_test_dummy".into(),
            stripe_webhook_secret: "wh".into(),
            database_url: "sqlite::memory:".into(),
            keys_dir: "keys".into(),
            content_dir: "content".into(),
            base_url: "http://x".into(),
            bind_addr: "127.0.0.1:0".into(),
            trust_proxy: false,
        };
        // Distinct per-product signing keys, so a product mix-up is detectable.
        let mut signing = std::collections::HashMap::new();
        signing.insert(
            "acme".to_string(),
            ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]),
        );
        signing.insert(
            "globex".to_string(),
            ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]),
        );
        AppState {
            cfg: std::sync::Arc::new(cfg),
            catalog: std::sync::Arc::new(test_catalog()),
            stripe: stripe::ClientBuilder::new("sk_test_dummy").build().unwrap(),
            signing: std::sync::Arc::new(signing),
            db: crate::db::connect("sqlite::memory:").await.unwrap(),
            neg_cache: std::sync::Arc::new(crate::cache::NegCache::new(
                std::time::Duration::from_secs(30),
                50_000,
            )),
            mail: None,
        }
    }

    fn paid_session(id: &str, sku: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            paid: true,
            sku: Some(sku.into()),
            email: Some("buyer@acme.example".into()),
            name: Some("Jane Buyer".into()),
            company: Some("Acme GmbH".into()),
            url: None,
            amount_total: Some(49900),
            currency: Some(stripe_types::Currency::EUR),
            // Matches the test key ("sk_test_dummy" -> expect livemode false).
            livemode: false,
        }
    }

    #[tokio::test]
    async fn paid_session_mints_once_and_is_stable() {
        let state = test_state().await;
        let s = paid_session("cs_1", "acme-business-annual");

        let first = super::fulfill_paid_session(&state, &s).await.unwrap();
        let rec = match first {
            FulfillOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        // The company custom field is the license identity, not the billing name.
        assert_eq!(rec.customer, "Acme GmbH");
        assert_eq!(rec.email, "buyer@acme.example");
        let blob = rec.blob;

        // Second call for the same session returns the identical stored blob.
        let second = super::fulfill_paid_session(&state, &s).await.unwrap();
        match second {
            FulfillOutcome::Ready(r) => assert_eq!(r.blob, blob),
            _ => panic!("expected Ready"),
        }
    }

    #[tokio::test]
    async fn underpaid_session_is_not_fulfilled() {
        let state = test_state().await;
        let mut s = paid_session("cs_underpaid", "acme-business-annual");
        s.amount_total = Some(49899);
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Pending
        ));
        s.amount_total = None;
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Pending
        ));
    }

    #[tokio::test]
    async fn currency_mismatch_is_not_fulfilled() {
        let state = test_state().await;
        let mut s = paid_session("cs_currency", "acme-business-annual");
        s.currency = Some(stripe_types::Currency::USD);
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Pending
        ));
        s.currency = None;
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Pending
        ));
    }

    #[tokio::test]
    async fn livemode_mismatch_is_not_fulfilled() {
        let state = test_state().await;
        let mut s = paid_session("cs_livemode", "acme-business-annual");
        // Test key configured, but a live-mode session arrives.
        s.livemode = true;
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Pending
        ));
    }

    #[test]
    fn payment_mismatch_accepts_exact_and_overpaid() {
        let cat = test_catalog();
        let sku = cat.by_id("acme-business-annual").unwrap();
        let s = paid_session("cs_ok", "acme-business-annual");
        assert_eq!(super::payment_mismatch(&s, sku, false), None);
        let mut over = paid_session("cs_over", "acme-business-annual");
        over.amount_total = Some(50000);
        assert_eq!(super::payment_mismatch(&over, sku, false), None);
        assert!(super::payment_mismatch(&s, sku, true).is_some());
    }

    #[tokio::test]
    async fn unpaid_session_is_pending() {
        let state = test_state().await;
        let mut s = paid_session("cs_2", "acme-business-annual");
        s.paid = false;
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Pending
        ));
    }

    /// Products are properly separated: each product's license is signed with
    /// that product's key and does NOT verify against the other product's key.
    #[tokio::test]
    async fn licenses_are_signed_per_product_and_do_not_cross_verify() {
        use signetlib::codec::decode_and_verify;

        let state = test_state().await;
        let acme_key = state.signing.get("acme").unwrap().verifying_key();
        let globex_key = state.signing.get("globex").unwrap().verifying_key();

        let ready = |o| match o {
            FulfillOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        let f = ready(
            super::fulfill_paid_session(&state, &paid_session("cs_f", "acme-business-annual"))
                .await
                .unwrap(),
        );
        let s = ready(
            super::fulfill_paid_session(&state, &paid_session("cs_s", "globex-pro-annual"))
                .await
                .unwrap(),
        );

        assert_eq!(f.product, "acme");
        assert_eq!(s.product, "globex");

        // Each blob verifies against its own product key only.
        assert!(decode_and_verify(&f.blob, &acme_key).is_ok());
        assert!(
            decode_and_verify(&f.blob, &globex_key).is_err(),
            "acme license must not verify with globex key"
        );
        assert!(decode_and_verify(&s.blob, &globex_key).is_ok());
        assert!(
            decode_and_verify(&s.blob, &acme_key).is_err(),
            "globex license must not verify with acme key"
        );
    }
}
