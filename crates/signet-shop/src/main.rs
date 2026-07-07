mod catalog;
mod config;
mod content;
mod db;
mod fulfill;
mod payments;
mod provision;
mod render;
mod routes;
mod state;
mod static_assets;

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
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
    let signing = state::load_signing_keys(&cfg, &catalog)?;
    let db = db::connect(&cfg.database_url).await?;
    let stripe = stripe::ClientBuilder::new(cfg.stripe_api_key.clone())
        .build()
        .context("build stripe client")?;
    let state = state::AppState {
        cfg: std::sync::Arc::new(cfg.clone()),
        catalog,
        stripe,
        signing: std::sync::Arc::new(signing),
        db,
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
