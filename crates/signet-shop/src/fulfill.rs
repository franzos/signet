use std::fmt;

use anyhow::Result;
use chrono::Utc;
use stripe_edge::{check_paid, Expect, Mismatch, SessionInfo};

use crate::catalog::{Sku, TaxMode};
use crate::db::{self, StoredLicense};
use crate::outbox;
use crate::state::AppState;

pub enum FulfillOutcome {
    /// Boxed: the record is large and the other variants are empty.
    Ready(Box<StoredLicense>),
    Pending,
    /// Paid, but held in `holds` until the operator releases it.
    Held,
    /// Stripe has no such session for this shop: unknown, long-expired or another service's id.
    LookupFailed,
}

/// Why a paid session is held instead of fulfilled. Displays as the stored
/// `holds.reason`, which never carries the session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldReason {
    MissingSku,
    UnknownSku,
    Mismatch(Mismatch),
    NoSigningKey,
    IssueFailed,
    /// The payment's charge could not be read.
    LookupFailed,
}

impl fmt::Display for HoldReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSku => f.write_str("missing_sku"),
            Self::UnknownSku => f.write_str("unknown_sku"),
            Self::Mismatch(m) => write!(f, "mismatch:{m}"),
            Self::NoSigningKey => f.write_str("no_signing_key"),
            Self::IssueFailed => f.write_str("issue_failed"),
            Self::LookupFailed => f.write_str("lookup_failed"),
        }
    }
}

/// What the payment's latest charge says about minting.
pub enum Payment {
    /// Not refunded or disputed, or the session has no payment intent.
    Clear,
    /// Refunded or disputed; carries the hold reason.
    Reversed(String),
    /// The charge could not be read and retrying will not help.
    Unknown,
}

/// Reads the session's charge. Only a retryable Stripe failure is an `Err`.
pub async fn check_payment(state: &AppState, session: &SessionInfo) -> Result<Payment> {
    let Some(pi) = session.payment_intent_id.as_deref() else {
        return Ok(Payment::Clear);
    };
    match (state.lookups.charge_for_intent)(state, pi).await {
        Ok(Some(c)) if c.refunded || c.amount_refunded >= c.amount => {
            Ok(Payment::Reversed(format!("refund:{}", c.id)))
        }
        Ok(Some(c)) if c.disputed => Ok(Payment::Reversed(format!("dispute:charge:{}", c.id))),
        Ok(Some(_)) => Ok(Payment::Clear),
        Ok(None) => {
            tracing::error!("payment intent has no charge");
            Ok(Payment::Unknown)
        }
        Err(e) if !e.retryable() => {
            tracing::error!("charge lookup failed: {e}");
            Ok(Payment::Unknown)
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether `metadata.service` names another app; untagged sessions are ours.
pub fn other_service(session: &SessionInfo) -> bool {
    session
        .metadata
        .get("service")
        .is_some_and(|svc| svc != crate::STRIPE_SERVICE)
}

pub async fn fulfill(state: &AppState, session_id: &str) -> Result<FulfillOutcome> {
    if let Some(existing) = db::get_by_session(&state.db, session_id).await? {
        return Ok(FulfillOutcome::Ready(Box::new(existing)));
    }
    if db::open_hold(&state.db, session_id).await?.is_some() {
        return Ok(FulfillOutcome::Held);
    }
    let Some(info) = (state.lookups.session)(state, session_id).await? else {
        return Ok(FulfillOutcome::LookupFailed);
    };
    if other_service(&info) {
        return Ok(FulfillOutcome::LookupFailed);
    }
    fulfill_paid_session(state, &info).await
}

pub async fn fulfill_paid_session(
    state: &AppState,
    session: &SessionInfo,
) -> Result<FulfillOutcome> {
    if !session.paid {
        return Ok(FulfillOutcome::Pending);
    }
    if let Some(existing) = db::get_by_session(&state.db, &session.id).await? {
        return Ok(FulfillOutcome::Ready(Box::new(existing)));
    }
    if db::open_hold(&state.db, &session.id).await?.is_some() {
        return Ok(FulfillOutcome::Held);
    }
    let prepared = match check_payment(state, session).await? {
        Payment::Clear => prepare(state, session, false),
        Payment::Unknown => Err(HoldReason::LookupFailed),
        Payment::Reversed(reason) => {
            if db::insert_hold(&state.db, &session.id, &reason, None).await? {
                tracing::warn!(%reason, "paid session held");
            }
            return Ok(FulfillOutcome::Held);
        }
    };
    match prepared {
        Ok(rec) => {
            let mail = outbox::rows_for(state, &rec);
            let (stored, _) = db::insert_or_get(&state.db, &rec, &mail).await?;
            Ok(FulfillOutcome::Ready(Box::new(stored)))
        }
        Err(reason) => {
            let reason = reason.to_string();
            let notify = outbox::hold_row(state, &session.id, &reason);
            if db::insert_hold(&state.db, &session.id, &reason, notify).await? {
                tracing::warn!(%reason, "paid session held");
            }
            Ok(FulfillOutcome::Held)
        }
    }
}

/// Build the licence record for a paid session without storing it.
/// `bypass_checks` skips the economics check only.
pub fn prepare(
    state: &AppState,
    session: &SessionInfo,
    bypass_checks: bool,
) -> Result<StoredLicense, HoldReason> {
    let sku_id = session.metadata.get("sku").ok_or(HoldReason::MissingSku)?;
    let sku: &Sku = state.catalog.by_id(sku_id).ok_or(HoldReason::UnknownSku)?;
    if !bypass_checks {
        let expect = Expect {
            min_amount: sku.amount_cents,
            currency: &sku.currency,
            livemode: state.stripe.livemode(),
        };
        check_paid(session, &expect).map_err(HoldReason::Mismatch)?;
    }
    let signing = state
        .signing
        .get(sku.category.as_str())
        .ok_or(HoldReason::NoSigningKey)?;

    let email = session.email.clone().unwrap_or_default();
    // The license identity is the company (checkout custom field), falling back
    // to the billing name if it is somehow absent.
    let customer = session
        .custom_field("company")
        .map(str::to_owned)
        .or_else(|| session.name.clone())
        .unwrap_or_default();

    // Under `mode = "none"` Stripe reports a flat zero tax, which would read as "no VAT due".
    let amount_tax = (state.catalog.tax.mode == TaxMode::Stripe).then_some(session.tax.amount_tax);
    // Stripe's `amount_subtotal` includes inclusive tax, so net is derived from the total.
    let amount_subtotal = amount_tax.zip(session.amount_total).map(|(t, a)| a - t);

    let now = Utc::now().timestamp();
    let params = sku.to_issue_params(
        customer.clone(),
        email.clone(),
        now,
        format!("web:{}", session.id),
    );
    let issued = signetlib::issue(params, now, signing).map_err(|e| {
        tracing::error!(sku = %sku.id, "license issue failed: {e}");
        HoldReason::IssueFailed
    })?;

    Ok(StoredLicense {
        stripe_session_id: session.id.clone(),
        license_id: issued.claims.license_id,
        product: sku.category.to_string(),
        sku: sku.id.to_string(),
        customer,
        email,
        blob: issued.blob,
        issued_at: now,
        amount_total: session.amount_total,
        amount_subtotal,
        amount_tax,
        currency: session.currency.clone(),
        country: session.tax.country.clone(),
        vat_id: session.tax.vat_id.clone(),
        tax_exemption: session.tax.exemption.clone(),
        revoked_at: None,
        revoked_reason: None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use stripe_edge::ChargeInfo;

    use super::*;
    use crate::state::StripeLookups;

    fn test_catalog() -> crate::catalog::Catalog {
        test_catalog_with("")
    }

    /// The shared catalog with `extra` (e.g. a `[tax]` section) prepended.
    fn test_catalog_with(extra: &str) -> crate::catalog::Catalog {
        crate::catalog::parse(&format!(
            r#"
{extra}
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
"#
        ))
        .unwrap()
    }

    pub(crate) async fn test_state() -> AppState {
        state_with_catalog(test_catalog()).await
    }

    async fn state_with_catalog(catalog: crate::catalog::Catalog) -> AppState {
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
            catalog: std::sync::Arc::new(catalog),
            stripe: stripe_edge::Stripe::offline("signet", false),
            signing: std::sync::Arc::new(signing),
            db: crate::db::connect("sqlite::memory:").await.unwrap(),
            mail: None,
            // Over the offline client every lookup fails as unreachable.
            lookups: crate::state::StripeLookups::live(),
        }
    }

    static SESSIONS: LazyLock<Mutex<HashMap<String, SessionInfo>>> =
        LazyLock::new(Default::default);

    /// Makes the stub session lookup return `session` for its id. Ids are shared across tests.
    pub(crate) fn stub_session(session: &SessionInfo) {
        SESSIONS
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
    }

    pub(crate) fn forget_session(id: &str) {
        SESSIONS.lock().unwrap().remove(id);
    }

    /// Sessions from `stub_session`, with `cs_test_permanent` a permanent error;
    /// charges by payment intent: `pi_refunded`, `pi_disputed`, `pi_permanent`,
    /// `pi_transient`, `pi_missing` (no charge), anything else a clean charge.
    /// Every charge id lookup fails permanently.
    pub(crate) fn stub_lookups() -> StripeLookups {
        StripeLookups {
            session: |_, id| {
                let found = match id {
                    "cs_test_permanent" => Err(permanent()),
                    _ => Ok(SESSIONS.lock().unwrap().get(id).cloned()),
                };
                Box::pin(async move { found })
            },
            charge: |_, _| Box::pin(async { Err(permanent()) }),
            charge_for_intent: |_, pi| {
                let charge = |refunded, disputed| ChargeInfo {
                    id: format!("ch_{}", pi.trim_start_matches("pi_")),
                    payment_intent_id: Some(pi.into()),
                    amount: 49900,
                    amount_refunded: if refunded { 49900 } else { 0 },
                    currency: "eur".into(),
                    refunded,
                    disputed,
                };
                let found = match pi {
                    "pi_refunded" => Ok(Some(charge(true, false))),
                    "pi_disputed" => Ok(Some(charge(false, true))),
                    "pi_permanent" => Err(permanent()),
                    "pi_transient" => Err(stripe_edge::Error::Transient("status 503".into())),
                    "pi_missing" => Ok(None),
                    _ => Ok(Some(charge(false, false))),
                };
                Box::pin(async move { found })
            },
        }
    }

    pub(crate) fn permanent() -> stripe_edge::Error {
        stripe_edge::Error::Permanent("status 404, code resource_missing".into())
    }

    /// The test state with the stub lookups.
    pub(crate) async fn stubbed_state() -> AppState {
        let mut state = test_state().await;
        state.lookups = stub_lookups();
        state
    }

    pub(crate) fn paid_session(id: &str, sku: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            mode: stripe_edge::Mode::Payment,
            status: stripe_edge::Status::Complete,
            paid: true,
            url: None,
            client_reference_id: None,
            metadata: [("sku".to_string(), sku.to_string())].into(),
            customer_id: None,
            subscription_id: None,
            payment_intent_id: None,
            email: Some("buyer@acme.example".into()),
            name: Some("Jane Buyer".into()),
            custom_fields: [("company".to_string(), "Acme GmbH".to_string())].into(),
            amount_total: Some(49900),
            currency: Some("eur".into()),
            // Matches `Stripe::offline("signet", false)`.
            livemode: false,
            tax: stripe_edge::TaxDetails::default(),
        }
    }

    /// A German consumer: 19% VAT inside a tax-inclusive price.
    fn taxed_session(id: &str, sku: &str) -> SessionInfo {
        let mut s = paid_session(id, sku);
        s.tax = stripe_edge::TaxDetails {
            amount_tax: 7967,
            country: Some("DE".into()),
            vat_id: None,
            exemption: None,
        };
        s
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

    const STRIPE_TAX: &str = r#"
[tax]
mode = "stripe"
seller_name = "Example Lda"
seller_vat_id = "PT999999990"
"#;

    #[tokio::test]
    async fn tax_mode_stripe_records_the_vat_split() {
        let state = state_with_catalog(test_catalog_with(STRIPE_TAX)).await;
        let rec = match super::fulfill_paid_session(
            &state,
            &taxed_session("cs_tax", "acme-business-annual"),
        )
        .await
        .unwrap()
        {
            FulfillOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        assert_eq!(rec.amount_total, Some(49900));
        assert_eq!(rec.amount_subtotal, Some(41933));
        assert_eq!(rec.amount_tax, Some(7967));
        assert_eq!(rec.currency.as_deref(), Some("eur"));
        assert_eq!(rec.country.as_deref(), Some("DE"));
    }

    #[tokio::test]
    async fn reverse_charge_records_the_vat_id_and_zero_tax() {
        let state = state_with_catalog(test_catalog_with(STRIPE_TAX)).await;
        let mut s = taxed_session("cs_rc", "acme-business-annual");
        s.tax.amount_tax = 0;
        s.tax.vat_id = Some("DE811907980".into());
        s.tax.exemption = Some("reverse".into());
        let rec = match super::fulfill_paid_session(&state, &s).await.unwrap() {
            FulfillOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        assert_eq!(rec.amount_tax, Some(0));
        assert_eq!(rec.amount_subtotal, Some(49900));
        assert_eq!(rec.vat_id.as_deref(), Some("DE811907980"));
        assert_eq!(rec.tax_exemption.as_deref(), Some("reverse"));
    }

    /// "Unknown" must not be recorded as a zero-VAT sale.
    #[tokio::test]
    async fn tax_mode_none_records_no_tax_amount() {
        let state = test_state().await;
        let rec = match super::fulfill_paid_session(
            &state,
            &paid_session("cs_notax", "acme-business-annual"),
        )
        .await
        .unwrap()
        {
            FulfillOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        assert_eq!(rec.amount_total, Some(49900));
        assert_eq!(rec.amount_tax, None);
        assert_eq!(rec.tax_exemption, None);
    }

    /// Fulfils `s`, expects `Held`, and returns the stored hold reason.
    async fn held_reason(state: &AppState, s: &SessionInfo) -> String {
        assert!(matches!(
            super::fulfill_paid_session(state, s).await.unwrap(),
            FulfillOutcome::Held
        ));
        assert!(db::get_by_session(&state.db, &s.id)
            .await
            .unwrap()
            .is_none());
        let reason = db::open_hold(&state.db, &s.id)
            .await
            .unwrap()
            .expect("hold row")
            .reason;
        assert!(!reason.contains(&s.id), "{reason}");
        reason
    }

    #[tokio::test]
    async fn underpaid_session_is_held() {
        let state = test_state().await;
        let mut s = paid_session("cs_underpaid", "acme-business-annual");
        s.amount_total = Some(49899);
        assert_eq!(
            held_reason(&state, &s).await,
            "mismatch:amount 49899 is below 49900"
        );
        let mut s = paid_session("cs_underpaid_none", "acme-business-annual");
        s.amount_total = None;
        assert_eq!(
            held_reason(&state, &s).await,
            "mismatch:amount missing, expected 49900"
        );
    }

    #[tokio::test]
    async fn currency_mismatch_is_held() {
        let state = test_state().await;
        let mut s = paid_session("cs_currency", "acme-business-annual");
        s.currency = Some("usd".into());
        assert_eq!(
            held_reason(&state, &s).await,
            "mismatch:currency usd does not match eur"
        );
        let mut s = paid_session("cs_currency_none", "acme-business-annual");
        s.currency = None;
        assert_eq!(
            held_reason(&state, &s).await,
            "mismatch:currency missing does not match eur"
        );
    }

    #[tokio::test]
    async fn livemode_mismatch_is_held_both_ways() {
        let state = test_state().await;
        let mut s = paid_session("cs_live", "acme-business-annual");
        s.livemode = true;
        assert_eq!(
            held_reason(&state, &s).await,
            "mismatch:livemode is true, expected false"
        );

        let mut state = test_state().await;
        state.stripe = stripe_edge::Stripe::offline("signet", true);
        let s = paid_session("cs_test", "acme-business-annual");
        assert_eq!(
            held_reason(&state, &s).await,
            "mismatch:livemode is false, expected true"
        );
    }

    #[tokio::test]
    async fn held_session_is_not_refetched_or_refulfilled() {
        let state = test_state().await;
        let mut s = paid_session("cs_held", "acme-business-annual");
        s.amount_total = Some(1);
        held_reason(&state, &s).await;
        // The offline client errors on any Stripe call, so `Held` proves none was made.
        assert!(matches!(
            super::fulfill(&state, "cs_held").await.unwrap(),
            FulfillOutcome::Held
        ));
        // A corrected replay stays held until the operator releases it.
        let s = paid_session("cs_held", "acme-business-annual");
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Held
        ));
    }

    #[tokio::test]
    async fn overpaid_session_is_fulfilled() {
        let state = test_state().await;
        let mut s = paid_session("cs_over", "acme-business-annual");
        s.amount_total = Some(50000);
        assert!(matches!(
            super::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Ready(_)
        ));
    }

    #[tokio::test]
    async fn unknown_sku_missing_sku_and_missing_key_are_held() {
        let state = test_state().await;
        assert_eq!(
            held_reason(&state, &paid_session("cs_u", "nope")).await,
            "unknown_sku"
        );
        let mut s = paid_session("cs_m", "acme-business-annual");
        s.metadata.clear();
        assert_eq!(held_reason(&state, &s).await, "missing_sku");

        let mut state = test_state().await;
        let mut signing = (*state.signing).clone();
        signing.remove("globex");
        state.signing = std::sync::Arc::new(signing);
        assert_eq!(
            held_reason(&state, &paid_session("cs_k", "globex-pro-annual")).await,
            "no_signing_key"
        );
    }

    #[tokio::test]
    async fn stripe_unreachable_is_a_retryable_error() {
        let state = test_state().await;
        let err = super::fulfill(&state, "cs_unknown").await.err().unwrap();
        assert!(
            err.downcast_ref::<stripe_edge::Error>()
                .is_some_and(stripe_edge::Error::retryable),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn another_services_session_is_not_fulfilled() {
        let mailer = crate::outbox::tests::TestMailer::default();
        let state = crate::outbox::tests::with_mail(stubbed_state().await, &mailer);
        let mut s = paid_session("cs_fulfill_formshive", "acme-business-annual");
        s.metadata.insert("service".into(), "formshive".into());
        stub_session(&s);
        assert!(matches!(
            super::fulfill(&state, &s.id).await.unwrap(),
            FulfillOutcome::LookupFailed
        ));
        assert!(db::open_hold(&state.db, &s.id).await.unwrap().is_none());
        assert!(db::get_by_session(&state.db, &s.id)
            .await
            .unwrap()
            .is_none());
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_outbox")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(queued, 0);
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
