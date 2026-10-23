mod catalog;
mod cli;
mod config;
mod content;
mod db;
mod events;
mod fulfill;
mod mail;
mod outbox;
mod provision;
mod render;
mod routes;
mod state;
mod static_assets;

use anyhow::Result;

/// `metadata.service` on this app's Stripe objects.
pub(crate) const STRIPE_SERVICE: &str = "signet";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            // async-stripe logs request paths, Checkout session ids included, at WARN.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,stripe=error".into()),
        )
        .init();

    let arg = |i| std::env::args().nth(i);
    match arg(1).as_deref() {
        Some("provision-stripe") => provision::run(arg(2).as_deref()).await,
        Some("holds") => cli::holds(arg(2).as_deref()).await,
        Some("release") => cli::release(arg(2).as_deref()).await,
        Some("dismiss") => cli::dismiss(arg(2).as_deref()).await,
        Some("reconcile") => cli::reconcile(arg(2).as_deref(), arg(3).as_deref()).await,
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
         signet-shop [serve]                       Run the web server (default)\n  \
         signet-shop provision-stripe [--dry-run]  Create Stripe products/prices from the catalog (--dry-run: report only)\n  \
         signet-shop holds [--all]                 List open holds (--all: resolved too), dead mail and revoked licences\n  \
         signet-shop release <session_id>          Issue the licence for a held, paid session\n  \
         signet-shop dismiss <id>                  Close an open hold without issuing a licence\n  \
         signet-shop reconcile --since YYYY-MM-DD  Fulfil paid Checkout Sessions created since the date (UTC)\n"
    );
}

async fn serve() -> Result<()> {
    let state = state::AppState::build(config::AppConfig::from_env()?).await?;
    if state.mail.is_some() {
        tokio::spawn(outbox::drain_loop(state.clone()));
    }
    let bind_addr = state.cfg.bind_addr.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("signet-shop listening on {bind_addr}");
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
