use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;

use crate::catalog::Catalog;
use crate::config::AppConfig;
use crate::mail::MailService;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<AppConfig>,
    pub catalog: Arc<Catalog>,
    pub stripe: stripe::Client,
    pub signing: Arc<HashMap<String, SigningKey>>,
    pub db: sqlx::SqlitePool,
    pub neg_cache: Arc<crate::cache::NegCache>,
    /// Purchase-email sender; `None` when no mail provider is configured.
    pub mail: Option<Arc<MailService>>,
}

/// Load `keys/<category>/private.bin` for every category referenced by the
/// catalog. Fails fast at startup if a key is missing or malformed.
pub fn load_signing_keys(
    cfg: &AppConfig,
    catalog: &Catalog,
) -> Result<HashMap<String, SigningKey>> {
    let mut map = HashMap::new();
    for category in catalog.category_ids() {
        let path = cfg.keys_dir.join(category).join("private.bin");
        let bytes =
            std::fs::read(&path).with_context(|| format!("read signing key {}", path.display()))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is not 32 bytes", path.display()))?;
        map.insert(category.to_string(), SigningKey::from_bytes(&arr));
    }
    Ok(map)
}
