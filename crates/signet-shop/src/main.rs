mod cache;
mod catalog;
mod config;
mod content;
mod db;
mod fulfill;
mod mail;
mod payments;
mod provision;
mod render;
mod routes;
mod state;
mod static_assets;

use anyhow::{Context, Result};

/// Install ring as the process-wide rustls provider. Two providers are linked
/// (async-stripe: ring, Lettermint's reqwest: aws-lc-rs), so rustls cannot pick
/// one from features and must be told explicitly, once, before any TLS is set
/// up. Idempotent: a second call is a no-op.
pub(crate) fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::main]
async fn main() -> Result<()> {
    ensure_crypto_provider();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("provision-stripe") => provision::run().await,
        None | Some("serve") => serve().await,
        Some("--help" | "-h" | "help") => {
            print_help();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    eprintln!(
        "signet-shop - Stripe license shop\n\n\
         USAGE:\n  \
         signet-shop [serve]            Run the web server (default)\n  \
         signet-shop provision-stripe   Create Stripe products/prices from the catalog\n"
    );
}

async fn serve() -> Result<()> {
    let cfg = config::AppConfig::from_env()?;
    let catalog = std::sync::Arc::new(catalog::load(&catalog::default_path())?);
    catalog.ensure_provisioned()?;
    let signing = state::load_signing_keys(&cfg, &catalog)?;
    let db = db::connect(&cfg.database_url).await?;
    let stripe = stripe::ClientBuilder::new(cfg.stripe_api_key.clone())
        .build()
        .context("build stripe client")?;
    let neg_cache = std::sync::Arc::new(cache::NegCache::new(
        std::time::Duration::from_secs(30),
        50_000,
    ));
    let mail = match mail::EmailConfig::load(&catalog::default_path(), &|k| std::env::var(k).ok())?
    {
        Some(ec) if ec.enabled => {
            let svc = mail::MailService::from_config(ec).context("build mail service")?;
            Some(std::sync::Arc::new(svc))
        }
        Some(_) => {
            tracing::info!("email is configured but disabled (enabled = false)");
            None
        }
        None => {
            tracing::warn!("no email provider configured; purchase emails are disabled");
            None
        }
    };
    let state = state::AppState {
        cfg: std::sync::Arc::new(cfg.clone()),
        catalog,
        stripe,
        signing: std::sync::Arc::new(signing),
        db,
        neg_cache,
        mail,
    };

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!("signet-shop listening on {}", cfg.bind_addr);
    // `into_make_service_with_connect_info::<SocketAddr>()` is required so the
    // rate limiter's PeerIpKeyExtractor can read the peer IP; plain
    // `into_make_service` omits ConnectInfo and every limited request 500s.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
