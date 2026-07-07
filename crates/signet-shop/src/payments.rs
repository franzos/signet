//! Stripe access via async-stripe 1.0-rc. `SessionInfo` extracts just what
//! fulfillment needs from a Checkout Session, so the rest of the app does not
//! depend on the SDK types.

use anyhow::{Context, Result};
use stripe_checkout::checkout_session::{
    CreateCheckoutSession, CreateCheckoutSessionCustomFields,
    CreateCheckoutSessionCustomFieldsLabel, CreateCheckoutSessionCustomFieldsLabelType,
    CreateCheckoutSessionCustomFieldsType, CreateCheckoutSessionLineItems, RetrieveCheckoutSession,
};
use stripe_checkout::CheckoutSessionMode;
use stripe_product::price::{CreatePrice, ListPrice, RetrievePrice, UpdatePrice};
use stripe_product::product::{CreateProduct, SearchProduct};
use stripe_shared::{CheckoutSession, CheckoutSessionPaymentStatus};
use stripe_types::Currency;

pub struct SessionInfo {
    pub id: String,
    pub paid: bool,
    pub sku: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    /// Company name, from the checkout "company" custom field.
    pub company: Option<String>,
    pub url: Option<String>,
}

impl SessionInfo {
    pub fn from_session(s: &CheckoutSession) -> Self {
        // Stripe's fulfillment guide treats both of these as paid;
        // `no_payment_required` covers a zero-amount session (100% coupon).
        let paid = matches!(
            s.payment_status,
            CheckoutSessionPaymentStatus::Paid | CheckoutSessionPaymentStatus::NoPaymentRequired
        );
        let (email, name) = match &s.customer_details {
            Some(d) => (d.email.clone(), d.name.clone()),
            None => (None, None),
        };
        let company = s
            .custom_fields
            .iter()
            .find(|f| f.key == "company")
            .and_then(|f| f.text.as_ref())
            .and_then(|t| t.value.clone());
        Self {
            id: s.id.to_string(),
            paid,
            sku: s.metadata.as_ref().and_then(|m| m.get("sku").cloned()),
            email,
            name,
            company,
            url: s.url.clone(),
        }
    }
}

pub async fn create_checkout_session(
    client: &stripe::Client,
    price_id: &str,
    sku_id: &str,
    success_url: &str,
    cancel_url: &str,
) -> Result<SessionInfo> {
    // No `payment_method_types`: keeps dynamic payment methods on (Dashboard).
    let line_items = vec![CreateCheckoutSessionLineItems {
        quantity: Some(1),
        price: Some(price_id.to_string()),
        ..Default::default()
    }];
    // Collect the buyer's company on Stripe's hosted page (email is always
    // collected). It comes back on the session as a custom field and is
    // embedded in the license as the `customer`. Required by default.
    let company_field = CreateCheckoutSessionCustomFields::new(
        "company",
        CreateCheckoutSessionCustomFieldsLabel::new(
            "Company name",
            CreateCheckoutSessionCustomFieldsLabelType::Custom,
        ),
        CreateCheckoutSessionCustomFieldsType::Text,
    );
    let session = CreateCheckoutSession::new()
        .mode(CheckoutSessionMode::Payment)
        .line_items(line_items)
        .custom_fields(vec![company_field])
        .success_url(success_url)
        .cancel_url(cancel_url)
        .metadata([(String::from("sku"), sku_id.to_string())])
        .send(client)
        .await
        .context("create checkout session")?;
    Ok(SessionInfo::from_session(&session))
}

pub async fn retrieve_session(client: &stripe::Client, session_id: &str) -> Result<SessionInfo> {
    let session = RetrieveCheckoutSession::new(session_id)
        .send(client)
        .await
        .context("retrieve checkout session")?;
    Ok(SessionInfo::from_session(&session))
}

/// Ensure a Stripe Product exists for `sku_id`, keyed on `metadata[sku]`.
/// Idempotent via product search, subject to Stripe's search index lag.
pub async fn ensure_product(client: &stripe::Client, sku_id: &str, name: &str) -> Result<String> {
    let query = format!("metadata['sku']:'{sku_id}'");
    let found = SearchProduct::new(query)
        .send(client)
        .await
        .context("search product")?;
    if let Some(p) = found.data.into_iter().next() {
        return Ok(p.id.to_string());
    }
    let product = CreateProduct::new(name)
        .metadata([(String::from("sku"), sku_id.to_string())])
        .send(client)
        .await
        .context("create product")?;
    Ok(product.id.to_string())
}

/// A minimal view of an existing Stripe Price, for reconciling the catalog file
/// against Stripe without leaking SDK types into the caller.
pub struct ExistingPrice {
    pub unit_amount: Option<i64>,
    pub currency: Currency,
    pub active: bool,
}

/// Retrieve a Price by id. `Ok(None)` when Stripe has no such price (a stale or
/// mistyped `stripe_price_id` in the catalog), so the caller can recreate it;
/// any other API error propagates.
pub async fn retrieve_price(
    client: &stripe::Client,
    price_id: &str,
) -> Result<Option<ExistingPrice>> {
    match RetrievePrice::new(price_id).send(client).await {
        Ok(p) => Ok(Some(ExistingPrice {
            unit_amount: p.unit_amount,
            currency: p.currency,
            active: p.active,
        })),
        Err(stripe::StripeError::Stripe(_, 404)) => Ok(None),
        Err(e) => Err(e).context("retrieve price"),
    }
}

/// Archive (deactivate) a Price so it is no longer purchasable. Stripe prices
/// are immutable, so a changed amount means a fresh price supersedes the old
/// one; we archive the old one to keep the product tidy.
pub async fn deactivate_price(client: &stripe::Client, price_id: &str) -> Result<()> {
    UpdatePrice::new(price_id)
        .active(false)
        .send(client)
        .await
        .context("deactivate price")?;
    Ok(())
}

/// Ensure a one-time Price (`unit_amount`/`currency`, no `recurring`) exists on
/// `product_id`, reusing an active matching one. Idempotent via the price list.
pub async fn ensure_price(
    client: &stripe::Client,
    product_id: &str,
    unit_amount: i64,
    currency: Currency,
) -> Result<String> {
    let list = ListPrice::new()
        .product(product_id.to_string())
        .active(true)
        .send(client)
        .await
        .context("list prices")?;
    if let Some(p) = list.data.into_iter().find(|p| {
        p.unit_amount == Some(unit_amount) && p.currency == currency && p.recurring.is_none()
    }) {
        return Ok(p.id.to_string());
    }
    let price = CreatePrice::new(currency)
        .product(product_id.to_string())
        .unit_amount(unit_amount)
        .send(client)
        .await
        .context("create price")?;
    Ok(price.id.to_string())
}

/// Integration tests against Stripe test mode. They run only when a test key is
/// present (`STRIPE_SECRET_KEY`, e.g. from `.formshive_stripe.env`); otherwise
/// they no-op so a credential-less `cargo test` still passes.
#[cfg(test)]
mod itests {
    use super::*;

    fn test_client() -> Option<stripe::Client> {
        let key = std::env::var("STRIPE_SECRET_KEY")
            .ok()
            .or_else(|| std::env::var("STRIPE_API_KEY").ok())?;
        stripe::ClientBuilder::new(key).build().ok()
    }

    #[tokio::test]
    async fn provision_and_checkout_roundtrip() {
        let Some(client) = test_client() else {
            eprintln!("skipping provision_and_checkout_roundtrip: no STRIPE_SECRET_KEY");
            return;
        };

        let product = ensure_product(&client, "itest-acme", "Integration Test SKU")
            .await
            .expect("ensure product");
        let price1 = ensure_price(&client, &product, 4900, Currency::EUR)
            .await
            .expect("ensure price");
        // Price list is strongly consistent, so a second ensure must reuse it.
        let price2 = ensure_price(&client, &product, 4900, Currency::EUR)
            .await
            .expect("ensure price again");
        assert_eq!(price1, price2, "ensure_price is not idempotent");

        // retrieve_price reflects the live price, and reports a stale id as gone.
        let existing = retrieve_price(&client, &price1)
            .await
            .expect("retrieve price")
            .expect("price should exist");
        assert_eq!(existing.unit_amount, Some(4900));
        assert_eq!(existing.currency, Currency::EUR);
        assert!(existing.active);
        assert!(
            retrieve_price(&client, "price_does_not_exist_zzz")
                .await
                .expect("retrieve missing price")
                .is_none(),
            "a nonexistent price id must reconcile as None"
        );

        let info = create_checkout_session(
            &client,
            &price1,
            "itest-acme",
            "https://example.com/success?session_id={CHECKOUT_SESSION_ID}",
            "https://example.com/",
        )
        .await
        .expect("create checkout session");
        assert!(info.id.starts_with("cs_"), "unexpected session id");
        assert!(info.url.is_some(), "hosted checkout should return a url");

        let fetched = retrieve_session(&client, &info.id)
            .await
            .expect("retrieve session");
        assert!(!fetched.paid, "a fresh session is not paid");
        assert_eq!(
            fetched.sku.as_deref(),
            Some("itest-acme"),
            "sku metadata should round-trip"
        );
    }
}
