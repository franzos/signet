//! Purchase notifications: a plain-text license email to the buyer and a sale
//! notice to the operator.
//!
//! Configuration follows the house email standard: polymail's `ProviderConfig`
//! (the `config` feature) is the transport/credential schema, wrapped in an
//! app-level `[email]` section that adds the sender identity and the operator
//! recipient. The base layer is the `[email]` table in `catalog.toml`;
//! environment variables (`SIGNET_EMAIL__*`) override it field by field, so
//! secrets stay blank in the file and are injected at runtime. Mail is optional:
//! no provider configured, or `enabled = false`, means the shop just skips it.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use polymail::{Address, Body, Email, Mailer, ProviderConfig};
use serde::Deserialize;

/// App-level email settings: sender identity and operator recipient on top of
/// polymail's flattened provider schema. No `derive(Debug)`: `ProviderConfig`'s
/// own `Debug` prints credentials verbatim, so this struct must never be logged.
#[derive(Deserialize)]
pub struct EmailConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub from_address: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub from_name: Option<String>,
    /// Recipient of the "a license was sold" notice; `None` disables that email.
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub operator_address: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub operator_name: Option<String>,
    #[serde(flatten)]
    pub provider: ProviderConfig,
}

fn default_true() -> bool {
    true
}

/// Treat a blank TOML/env value as absent, so `from_name = ""` means "unset"
/// rather than an empty display name.
fn empty_string_as_none<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(d)?;
    Ok(opt.filter(|s| !s.is_empty()))
}

impl EmailConfig {
    /// Load the `[email]` config: the `catalog.toml` table as the base, with
    /// `SIGNET_EMAIL__*` env vars overlaid on top (env wins). Returns `Ok(None)`
    /// when no provider is selected anywhere (mail disabled). A selected but
    /// incomplete provider fails to deserialize, so misconfiguration is caught
    /// at startup rather than on the first sale.
    pub fn load(catalog_path: &Path, get: &dyn Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let mut table = base_email_table(catalog_path)?;
        apply_env(&mut table, get)?;
        if !table.contains_key("provider") {
            return Ok(None);
        }
        let cfg: EmailConfig = toml::Value::Table(table)
            .try_into()
            .context("invalid [email] configuration")?;
        Ok(Some(cfg))
    }
}

/// The `[email]` table from `catalog.toml`, or an empty table when the file or
/// section is absent (an env-only configuration is valid).
fn base_email_table(catalog_path: &Path) -> Result<toml::Table> {
    let text = match std::fs::read_to_string(catalog_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", catalog_path.display())),
    };
    let root: toml::Table =
        toml::from_str(&text).with_context(|| format!("parse {}", catalog_path.display()))?;
    match root.get("email") {
        Some(toml::Value::Table(t)) => Ok(t.clone()),
        Some(_) => bail!("[email] in {} must be a table", catalog_path.display()),
        None => Ok(toml::Table::new()),
    }
}

/// Overlay `SIGNET_EMAIL__*` env vars onto the table, with the type each field
/// expects. Merging into the raw table before deserializing (rather than after)
/// keeps the flattened, internally-tagged `ProviderConfig` resolving correctly
/// whether a field came from TOML or env.
fn apply_env(table: &mut toml::Table, get: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    const STRING_FIELDS: &[(&str, &str)] = &[
        ("FROM_ADDRESS", "from_address"),
        ("FROM_NAME", "from_name"),
        ("OPERATOR_ADDRESS", "operator_address"),
        ("OPERATOR_NAME", "operator_name"),
        ("PROVIDER", "provider"),
        ("TOKEN", "token"),
        ("HOST", "host"),
        ("TLS", "tls"),
        ("USER", "user"),
        ("PASS", "pass"),
    ];
    for (suffix, key) in STRING_FIELDS {
        if let Some(v) = get(&format!("SIGNET_EMAIL__{suffix}")) {
            table.insert((*key).to_string(), toml::Value::String(v));
        }
    }
    if let Some(v) = get("SIGNET_EMAIL__PORT") {
        let port: i64 = v
            .parse()
            .with_context(|| format!("SIGNET_EMAIL__PORT is not a valid port: {v:?}"))?;
        table.insert("port".to_string(), toml::Value::Integer(port));
    }
    if let Some(v) = get("SIGNET_EMAIL__ENABLED") {
        let on = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        table.insert("enabled".to_string(), toml::Value::Boolean(on));
    }
    Ok(())
}

/// Everything a purchase notification needs, owned so it can outlive the request
/// on a background task.
pub struct PurchaseNotice {
    pub buyer_email: String,
    pub customer: String,
    pub product_display: String,
    pub license_id: String,
    pub license_blob: String,
    pub expires_at: Option<i64>,
    pub price_label: String,
}

/// A constructed mailer plus the addresses it sends from/to.
pub struct MailService {
    mailer: Box<dyn Mailer>,
    from: Address,
    operator: Option<Address>,
}

impl MailService {
    /// Build a ready mailer from the loaded config, validating and surfacing
    /// transport/credential errors now rather than on the first send. Must run
    /// inside a Tokio runtime: the pooled SMTP transport captures the current
    /// runtime handle at build time.
    pub fn from_config(cfg: EmailConfig) -> Result<Self> {
        let from_address = cfg
            .from_address
            .clone()
            .ok_or_else(|| anyhow!("email is enabled but from_address is not set"))?;
        // ProviderConfig accepts an empty token (env is meant to fill it), so
        // reject an unfilled API credential explicitly.
        if let ProviderConfig::Lettermint { token } = &cfg.provider {
            if token.is_empty() {
                bail!("lettermint provider selected but token is empty (set SIGNET_EMAIL__TOKEN)");
            }
        }
        let from = to_address(from_address, cfg.from_name);
        let operator = cfg
            .operator_address
            .map(|addr| to_address(addr, cfg.operator_name));
        let mailer = cfg
            .provider
            .build()
            .map_err(|e| anyhow!("build mailer: {e}"))?;
        Ok(Self {
            mailer,
            from,
            operator,
        })
    }

    /// Send both notifications, logging (never propagating) any failure so a mail
    /// outage can never block or reverse license delivery.
    pub async fn notify_purchase(&self, n: &PurchaseNotice) {
        if n.buyer_email.is_empty() {
            tracing::warn!("purchase has no buyer email; skipping license email");
        } else {
            match self.build_license_email(n) {
                Ok(email) => {
                    if let Err(e) = self.mailer.send(&email).await {
                        tracing::error!("failed to send license email: {e}");
                    }
                }
                Err(e) => tracing::error!("failed to build license email: {e}"),
            }
        }

        if let Some(operator) = &self.operator {
            match self.build_operator_email(operator, n) {
                Ok(email) => {
                    if let Err(e) = self.mailer.send(&email).await {
                        tracing::error!("failed to send operator notice: {e}");
                    }
                }
                Err(e) => tracing::error!("failed to build operator notice: {e}"),
            }
        }
    }

    fn build_license_email(&self, n: &PurchaseNotice) -> Result<Email> {
        let body = format!(
            "Hi,\n\n\
             thank you for your purchase. Your {product} license is below.\n\n\
             License key:\n{blob}\n\n\
             License ID: {id}\n\
             Expires:    {expiry}\n\n\
             Keep this key safe; it is your proof of license.\n",
            product = n.product_display,
            blob = n.license_blob,
            id = n.license_id,
            expiry = format_expiry(n.expires_at),
        );
        let to = to_address(
            n.buyer_email.clone(),
            (!n.customer.is_empty()).then(|| n.customer.clone()),
        );
        Email::builder(
            self.from.clone(),
            format!("Your {} license", n.product_display),
            Body::Text(body),
        )
        .to(to)
        .build()
        .map_err(|e| anyhow!("{e}"))
    }

    fn build_operator_email(&self, operator: &Address, n: &PurchaseNotice) -> Result<Email> {
        let buyer = if n.buyer_email.is_empty() {
            "(none)"
        } else {
            &n.buyer_email
        };
        let body = format!(
            "A new license was sold.\n\n\
             Product:     {product}\n\
             Customer:    {customer}\n\
             Buyer email: {buyer}\n\
             Price:       {price}\n\
             License ID:  {id}\n\
             Expires:     {expiry}\n",
            product = n.product_display,
            customer = n.customer,
            buyer = buyer,
            price = n.price_label,
            id = n.license_id,
            expiry = format_expiry(n.expires_at),
        );
        Email::builder(
            self.from.clone(),
            format!("New license sold: {}", n.product_display),
            Body::Text(body),
        )
        .to(operator.clone())
        .build()
        .map_err(|e| anyhow!("{e}"))
    }
}

/// An `Address` with an optional, non-blank display name.
fn to_address(email: String, name: Option<String>) -> Address {
    match name.filter(|n| !n.is_empty()) {
        Some(n) => Address::with_name(email, n),
        None => Address::new(email),
    }
}

/// `expires_at` (Unix seconds) as a date, or a lifetime marker when absent.
fn format_expiry(expires_at: Option<i64>) -> String {
    match expires_at {
        None => "Lifetime (no expiry)".to_string(),
        Some(ts) => match chrono::DateTime::from_timestamp(ts, 0) {
            Some(dt) => dt.format("%Y-%m-%d").to_string(),
            None => "unknown".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymail::provider::lettermint::LettermintMailer;

    fn notice() -> PurchaseNotice {
        PurchaseNotice {
            buyer_email: "buyer@acme.example".into(),
            customer: "Acme GmbH".into(),
            product_display: "Acme Business (annual)".into(),
            license_id: "lic_123".into(),
            license_blob: "SIGNET.blob.here".into(),
            expires_at: Some(1_700_000_000),
            price_label: "EUR 499 / year".into(),
        }
    }

    fn service() -> MailService {
        MailService {
            mailer: Box::new(LettermintMailer::new("test-token")),
            from: Address::with_name("shop@example.com", "Example Shop"),
            operator: Some(Address::new("ops@example.com")),
        }
    }

    /// A getter over a fixed map, standing in for the environment.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn load_disabled_when_no_provider() {
        // A path that does not exist stands in for "no catalog / no [email]".
        let missing = Path::new("/nonexistent/catalog.toml");
        let cfg = EmailConfig::load(missing, &env(&[])).unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn load_env_only_lettermint() {
        let missing = Path::new("/nonexistent/catalog.toml");
        let get = env(&[
            ("SIGNET_EMAIL__PROVIDER", "lettermint"),
            ("SIGNET_EMAIL__TOKEN", "lm_secret"),
            ("SIGNET_EMAIL__FROM_ADDRESS", "shop@example.com"),
            ("SIGNET_EMAIL__OPERATOR_ADDRESS", "ops@example.com"),
        ]);
        let cfg = EmailConfig::load(missing, &get).unwrap().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.from_address.as_deref(), Some("shop@example.com"));
        assert_eq!(cfg.operator_address.as_deref(), Some("ops@example.com"));
        match cfg.provider {
            ProviderConfig::Lettermint { token } => assert_eq!(token, "lm_secret"),
            _ => panic!("expected lettermint provider"),
        }
    }

    #[test]
    fn load_smtp_parses_port_and_tls_from_env() {
        let missing = Path::new("/nonexistent/catalog.toml");
        let get = env(&[
            ("SIGNET_EMAIL__PROVIDER", "smtp"),
            ("SIGNET_EMAIL__HOST", "smtp.example.com"),
            ("SIGNET_EMAIL__PORT", "587"),
            ("SIGNET_EMAIL__TLS", "start_tls"),
            ("SIGNET_EMAIL__USER", "u"),
            ("SIGNET_EMAIL__PASS", "p"),
            ("SIGNET_EMAIL__FROM_ADDRESS", "shop@example.com"),
        ]);
        let cfg = EmailConfig::load(missing, &get).unwrap().unwrap();
        match cfg.provider {
            ProviderConfig::Smtp {
                host,
                port,
                tls,
                user,
                pass,
            } => {
                assert_eq!(host, "smtp.example.com");
                assert_eq!(port, Some(587));
                assert_eq!(tls, polymail::provider::smtp::SmtpTls::StartTls);
                assert_eq!(user.as_deref(), Some("u"));
                assert_eq!(pass.as_deref(), Some("p"));
            }
            _ => panic!("expected smtp provider"),
        }
    }

    #[test]
    fn env_overrides_toml_secret() {
        // Base [email] table with a blank token; env fills it (env wins).
        let dir = std::env::temp_dir().join("signet-mail-test-envwins");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("catalog.toml");
        std::fs::write(
            &path,
            "[email]\nfrom_address = \"shop@example.com\"\nprovider = \"lettermint\"\ntoken = \"\"\n",
        )
        .unwrap();
        let cfg = EmailConfig::load(&path, &env(&[("SIGNET_EMAIL__TOKEN", "lm_from_env")]))
            .unwrap()
            .unwrap();
        match cfg.provider {
            ProviderConfig::Lettermint { token } => assert_eq!(token, "lm_from_env"),
            _ => panic!("expected lettermint provider"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_config_rejects_missing_from_address() {
        let cfg = EmailConfig {
            enabled: true,
            from_address: None,
            from_name: None,
            operator_address: None,
            operator_name: None,
            provider: ProviderConfig::Lettermint {
                token: "tok".into(),
            },
        };
        assert!(MailService::from_config(cfg).is_err());
    }

    #[test]
    fn from_config_rejects_empty_lettermint_token() {
        let cfg = EmailConfig {
            enabled: true,
            from_address: Some("shop@example.com".into()),
            from_name: None,
            operator_address: None,
            operator_name: None,
            provider: ProviderConfig::Lettermint { token: "".into() },
        };
        assert!(MailService::from_config(cfg).is_err());
    }

    // A tokio runtime must be live: the SMTP transport captures the current
    // runtime handle at build time (as it does under `serve`'s runtime).
    #[tokio::test]
    async fn from_config_builds_smtp_and_lettermint() {
        crate::ensure_crypto_provider();
        let smtp = EmailConfig {
            enabled: true,
            from_address: Some("shop@example.com".into()),
            from_name: None,
            operator_address: None,
            operator_name: None,
            provider: ProviderConfig::Smtp {
                host: "smtp.example.com".into(),
                port: Some(587),
                tls: polymail::provider::smtp::SmtpTls::StartTls,
                user: Some("u".into()),
                pass: Some("p".into()),
            },
        };
        assert!(MailService::from_config(smtp).is_ok());

        let lettermint = EmailConfig {
            enabled: true,
            from_address: Some("shop@example.com".into()),
            from_name: Some("Shop".into()),
            operator_address: Some("ops@example.com".into()),
            operator_name: None,
            provider: ProviderConfig::Lettermint {
                token: "tok".into(),
            },
        };
        assert!(MailService::from_config(lettermint).is_ok());
    }

    #[test]
    fn license_email_carries_key_and_expiry() {
        let svc = service();
        let email = svc.build_license_email(&notice()).unwrap();
        assert_eq!(email.subject, "Your Acme Business (annual) license");
        assert_eq!(email.from.email, "shop@example.com");
        assert_eq!(email.to.len(), 1);
        assert_eq!(email.to[0].email, "buyer@acme.example");
        let text = match &email.body {
            Body::Text(t) => t,
            _ => panic!("expected plain text body"),
        };
        assert!(text.contains("SIGNET.blob.here"));
        assert!(text.contains("lic_123"));
        assert!(text.contains("2023-11-14")); // 1_700_000_000 -> 2023-11-14
    }

    #[test]
    fn lifetime_license_shows_no_expiry() {
        let svc = service();
        let mut n = notice();
        n.expires_at = None;
        let email = svc.build_license_email(&n).unwrap();
        let text = match &email.body {
            Body::Text(t) => t,
            _ => panic!("expected plain text body"),
        };
        assert!(text.contains("Lifetime (no expiry)"));
    }

    #[test]
    fn operator_email_addresses_operator() {
        let svc = service();
        let email = svc
            .build_operator_email(svc.operator.as_ref().unwrap(), &notice())
            .unwrap();
        assert_eq!(email.subject, "New license sold: Acme Business (annual)");
        assert_eq!(email.to[0].email, "ops@example.com");
        let text = match &email.body {
            Body::Text(t) => t,
            _ => panic!("expected plain text body"),
        };
        assert!(text.contains("Acme GmbH"));
        assert!(text.contains("buyer@acme.example"));
        assert!(text.contains("EUR 499 / year"));
    }
}
