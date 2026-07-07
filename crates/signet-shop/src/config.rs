use std::fmt;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

#[derive(Clone)]
pub struct AppConfig {
    pub stripe_api_key: String,
    pub stripe_webhook_secret: String,
    pub database_url: String,
    pub keys_dir: PathBuf,
    pub content_dir: PathBuf,
    pub base_url: String,
    pub bind_addr: String,
    pub trust_proxy: bool,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("stripe_api_key", &"[redacted]")
            .field("stripe_webhook_secret", &"[redacted]")
            .field("database_url", &self.database_url)
            .field("keys_dir", &self.keys_dir)
            .field("content_dir", &self.content_dir)
            .field("base_url", &self.base_url)
            .field("bind_addr", &self.bind_addr)
            .field("trust_proxy", &self.trust_proxy)
            .finish()
    }
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_getter(&|k| std::env::var(k).ok())
    }

    /// Split out for testability; `get` returns an env var or `None`.
    pub fn from_getter(get: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let req = |k: &str| get(k).ok_or_else(|| anyhow!("missing env var {k}"));
        Ok(Self {
            stripe_api_key: req("STRIPE_API_KEY")?,
            stripe_webhook_secret: req("STRIPE_WEBHOOK_SECRET")?,
            database_url: req("DATABASE_URL")?,
            base_url: req("BASE_URL")?,
            keys_dir: get("KEYS_DIR").unwrap_or_else(|| "./keys".into()).into(),
            content_dir: get("CONTENT_DIR")
                .unwrap_or_else(|| "./content".into())
                .into(),
            bind_addr: get("BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into()),
            trust_proxy: get("TRUST_PROXY")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_reads_required_and_defaults() {
        let get = |k: &str| -> Option<String> {
            match k {
                "STRIPE_API_KEY" => Some("rk_test_x".into()),
                "STRIPE_WEBHOOK_SECRET" => Some("whsec_x".into()),
                "DATABASE_URL" => Some("sqlite::memory:".into()),
                "BASE_URL" => Some("https://buy.example".into()),
                _ => None,
            }
        };
        let cfg = AppConfig::from_getter(&get).unwrap();
        assert_eq!(cfg.base_url, "https://buy.example");
        assert_eq!(cfg.keys_dir.to_str().unwrap(), "./keys");
        assert_eq!(cfg.bind_addr, "127.0.0.1:8080");
    }

    #[test]
    fn from_env_errors_on_missing_secret() {
        let get = |_: &str| None;
        assert!(AppConfig::from_getter(&get).is_err());
    }
}
