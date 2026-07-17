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
            base_url: validate_base_url(req("BASE_URL")?)?,
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

/// BASE_URL ends up in Stripe success/cancel URLs; a plain-http value would
/// send the session id bearer secret over the wire in clear. Trailing slashes
/// are trimmed so `{base_url}/success` never yields `//success` (a 404).
fn validate_base_url(url: String) -> Result<String> {
    let trimmed = || url.trim_end_matches('/').to_string();
    if url.starts_with("https://") {
        return Ok(trimmed());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if host == "localhost" || host == "127.0.0.1" {
            return Ok(trimmed());
        }
    }
    Err(anyhow!(
        "BASE_URL must be https:// (http:// is allowed only for localhost/127.0.0.1): {url}"
    ))
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

    #[test]
    fn base_url_requires_https_except_localhost() {
        for ok in [
            "https://buy.example",
            "http://localhost",
            "http://localhost:8080",
            "http://127.0.0.1:8080/shop",
        ] {
            assert!(validate_base_url(ok.into()).is_ok(), "{ok} should pass");
        }
        for bad in [
            "http://buy.example",
            "http://localhost.evil.example",
            "ftp://buy.example",
            "buy.example",
        ] {
            assert!(validate_base_url(bad.into()).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn base_url_trims_trailing_slashes() {
        assert_eq!(
            validate_base_url("https://buy.example/".into()).unwrap(),
            "https://buy.example"
        );
        assert_eq!(
            validate_base_url("https://buy.example//".into()).unwrap(),
            "https://buy.example"
        );
        assert_eq!(
            validate_base_url("http://localhost:8080/".into()).unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            validate_base_url("https://buy.example/shop".into()).unwrap(),
            "https://buy.example/shop"
        );
    }
}
