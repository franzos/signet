use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use stripe_edge::{ChargeInfo, SessionInfo};

use crate::catalog::Catalog;
use crate::config::AppConfig;
use crate::mail::{EmailConfig, MailService};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<AppConfig>,
    pub catalog: Arc<Catalog>,
    pub stripe: stripe_edge::Stripe,
    pub signing: Arc<HashMap<String, SigningKey>>,
    pub db: sqlx::SqlitePool,
    /// Purchase-email sender; `None` when no mail provider is configured.
    pub mail: Option<Arc<MailService>>,
    /// The Stripe reads fulfilment depends on; `StripeLookups::live()` outside tests.
    pub lookups: StripeLookups,
}

pub type Lookup<'a, T> = Pin<Box<dyn Future<Output = Result<T, stripe_edge::Error>> + Send + 'a>>;

/// A Stripe read keyed by one id.
pub type LookupFn<T> = for<'a> fn(&'a AppState, &'a str) -> Lookup<'a, T>;

/// Stripe reads as plain function pointers, so tests can stub them.
#[derive(Clone, Copy)]
pub struct StripeLookups {
    /// A Checkout Session by id; `None` when Stripe has no such session.
    pub session: LookupFn<Option<SessionInfo>>,
    /// A charge by id with the Checkout Session that paid it.
    pub charge: LookupFn<Option<(SessionInfo, ChargeInfo)>>,
    /// The latest charge of a payment intent.
    pub charge_for_intent: LookupFn<Option<ChargeInfo>>,
}

impl StripeLookups {
    /// Calls through `AppState::stripe`.
    pub fn live() -> Self {
        Self {
            session: |state, id| Box::pin(state.stripe.retrieve_session(id)),
            charge: live_charge,
            charge_for_intent: |state, pi| Box::pin(state.stripe.charge_for_payment_intent(pi)),
        }
    }
}

/// Fetches the charge, then the session by its payment intent.
fn live_charge<'a>(
    state: &'a AppState,
    charge_id: &'a str,
) -> Lookup<'a, Option<(SessionInfo, ChargeInfo)>> {
    Box::pin(async move {
        let charge = state.stripe.charge(charge_id).await?;
        let Some(pi) = charge.payment_intent_id.as_deref() else {
            return Ok(None);
        };
        let session = state.stripe.session_for_payment_intent(pi).await?;
        Ok(session.map(|s| (s, charge)))
    })
}

impl AppState {
    /// Load the catalog and signing keys, open and migrate the database, connect
    /// to Stripe and build the mail service.
    pub async fn build(cfg: AppConfig) -> Result<Self> {
        let catalog_path = crate::catalog::default_path();
        let catalog = crate::catalog::load(&catalog_path)?;
        catalog.ensure_provisioned()?;
        let signing = load_signing_keys(&cfg, &catalog)?;
        let db = crate::db::connect(&cfg.database_url).await?;
        let stripe = stripe_edge::Stripe::connect(&cfg.stripe_api_key, crate::STRIPE_SERVICE)
            .await
            .context("connect to Stripe")?;
        tracing::info!(livemode = stripe.livemode(), "connected to Stripe");
        let mail = match EmailConfig::load(&catalog_path, &|k| std::env::var(k).ok())? {
            Some(ec) if ec.enabled => Some(Arc::new(
                MailService::from_config(ec).context("build mail service")?,
            )),
            Some(_) => {
                tracing::info!("email is configured but disabled (enabled = false)");
                None
            }
            None => {
                tracing::warn!("no email provider configured; purchase emails are disabled");
                None
            }
        };
        Ok(Self {
            cfg: Arc::new(cfg),
            catalog: Arc::new(catalog),
            stripe,
            signing: Arc::new(signing),
            db,
            mail,
            lookups: StripeLookups::live(),
        })
    }
}

/// Load `keys/<category>/web-private.bin` (the revocable web signing key, never
/// the offline root `private.bin`) for every category referenced by the
/// catalog. Fails fast at startup if a key is missing or malformed.
pub fn load_signing_keys(
    cfg: &AppConfig,
    catalog: &Catalog,
) -> Result<HashMap<String, SigningKey>> {
    let mut map = HashMap::new();
    for category in catalog.category_ids() {
        let path = cfg.keys_dir.join(category).join("web-private.bin");
        let key = signetlib::codec::load_signing_key(&path).with_context(|| {
            format!(
                "load web signing key {}: run `signet-issuer keygen --product {category} --web` \
                 and mount only web keys here (the root private.bin stays offline)",
                path.display()
            )
        })?;
        map.insert(category.to_string(), key);
    }
    Ok(map)
}
