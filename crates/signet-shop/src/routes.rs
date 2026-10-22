use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use stripe_edge::{CheckoutSpec, CustomField, Line, Mode};

use crate::render::page;
use crate::state::AppState;
use crate::{catalog, events, fulfill};

pub fn router(state: AppState) -> Router {
    use axum::extract::DefaultBodyLimit;
    use std::sync::Arc;
    use tower_governor::governor::GovernorConfigBuilder;
    use tower_governor::key_extractor::SmartIpKeyExtractor;
    use tower_governor::GovernorLayer;
    use tower_http::set_header::SetResponseHeaderLayer;

    // Per-IP limiter on the endpoints that reach Stripe (denial-of-wallet). The
    // key extractor is selected by TRUST_PROXY: the default PeerIpKeyExtractor
    // keys on the direct peer (requires `into_make_service_with_connect_info`),
    // which behind a proxy/LB collapses to one global bucket. Set TRUST_PROXY for
    // trusted-proxy deployments to use SmartIpKeyExtractor (X-Forwarded-For);
    // never enable it when directly exposed, as that header is then spoofable.
    let peer_cfg = GovernorConfigBuilder::default()
        .per_second(2)
        .burst_size(5)
        .finish()
        .expect("valid governor config");
    let smart_cfg = GovernorConfigBuilder::default()
        .per_second(2)
        .burst_size(5)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .expect("valid governor config");
    // The keyed limiters never evict idle keys on their own; prune both
    // periodically so the per-IP state map stays bounded.
    let peer_limiter = peer_cfg.limiter().clone();
    let smart_limiter = smart_cfg.limiter().clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            peer_limiter.retain_recent();
            smart_limiter.retain_recent();
        }
    });
    // Routes defined once; the two branches only differ in the layer's key
    // extractor type, and `.layer()` erases the service so both are Router<AppState>.
    let limited_base = || {
        Router::new()
            .route("/checkout", post(checkout))
            .route("/success", get(success))
            .route("/success/download", get(success_download))
    };
    let limited = if state.cfg.trust_proxy {
        limited_base().layer(GovernorLayer::new(Arc::new(smart_cfg)))
    } else {
        limited_base().layer(GovernorLayer::new(Arc::new(peer_cfg)))
    };

    // Self-hosted static UI plus a redirect out to Stripe. The Buy form POSTs to
    // /checkout, which 303-redirects to checkout.stripe.com; browsers apply
    // `form-action` to the redirect destination too, so it must list Stripe's
    // hosted checkout domain or the jump is blocked. A configured analytics host
    // is added to script-src/connect-src so the strict policy allows just it.
    let extra = state
        .catalog
        .analytics
        .as_ref()
        .and_then(|a| analytics_origin(&a.src))
        .map(|o| format!(" {o}"))
        .unwrap_or_default();
    let csp = format!(
        "default-src 'self'; img-src 'self' data:; base-uri 'none'; \
         script-src 'self'{extra}; connect-src 'self'{extra}; \
         form-action 'self' https://checkout.stripe.com; frame-ancestors 'none'"
    );
    let csp = HeaderValue::from_str(&csp).expect("valid CSP header");

    Router::new()
        .route("/", get(catalog_page))
        .route("/p/{slug}", get(page_view))
        .route("/{category}", get(category_page))
        .route(
            "/webhook",
            post(webhook).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/static/{*path}", get(crate::static_assets::serve))
        .merge(limited)
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        // Add HSTS only when this process terminates TLS:
        // .layer(SetResponseHeaderLayer::overriding(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000; includeSubDomains")))
        .with_state(state)
}

/// Origin (`https://host[:port]`) of an analytics `src`, for the CSP allowlist.
fn analytics_origin(src: &str) -> Option<String> {
    let host = src.strip_prefix("https://")?.split('/').next()?;
    (!host.is_empty()).then(|| format!("https://{host}"))
}

/// Resolve the appearance preference from the `license_theme` cookie. Rendered
/// into `<html data-theme>` so the CSS picks the palette before first paint
/// (System follows the OS via `prefers-color-scheme`). Anything unrecognised
/// falls back to System.
fn theme_pref(headers: &HeaderMap) -> &'static str {
    let Some(cookies) = headers.get("cookie").and_then(|v| v.to_str().ok()) else {
        return "system";
    };
    for kv in cookies.split(';') {
        if let Some(v) = kv.trim().strip_prefix("license_theme=") {
            return match v {
                "light" => "light",
                "dark" => "dark",
                _ => "system",
            };
        }
    }
    "system"
}

const DEFAULT_PAYMENT_NOTICE: &str =
    "Payments are processed by Stripe, which collects your email, company name and payment details.";

/// Site chrome shared by every page: theme, footer, and analytics. Built once
/// per request from the catalog (owned so templates carry no lifetime).
struct Chrome {
    theme: &'static str,
    notice: Option<String>,
    links: Vec<ChromeLink>,
    analytics: Option<ChromeAnalytics>,
}

struct ChromeLink {
    title: String,
    href: String,
}

struct ChromeAnalytics {
    src: String,
    entity: Option<String>,
    module: bool,
}

fn chrome(state: &AppState, headers: &HeaderMap) -> Chrome {
    let cat = &state.catalog;
    let mut links = Vec::new();
    // Footer pages first (only if their markdown actually exists), then links.
    for p in &cat.pages {
        if p.footer && crate::content::exists(&state.cfg.content_dir, &p.slug) {
            links.push(ChromeLink {
                title: p.title.clone(),
                href: format!("/p/{}", p.slug),
            });
        }
    }
    for l in &cat.footer_links {
        links.push(ChromeLink {
            title: l.title.clone(),
            href: l.url.clone(),
        });
    }
    // None -> default notice; explicit "" -> hidden.
    let notice = match cat.shop.payment_notice.as_deref() {
        Some("") => None,
        Some(s) => Some(s.to_string()),
        None => Some(DEFAULT_PAYMENT_NOTICE.to_string()),
    };
    let analytics = cat.analytics.as_ref().map(|a| ChromeAnalytics {
        src: a.src.clone(),
        entity: a.entity.clone(),
        module: a.module,
    });
    Chrome {
        theme: theme_pref(headers),
        notice,
        links,
        analytics,
    }
}

/// Guard every attacker-reachable session id before it reaches Stripe, the DB,
/// or a template. Matches Stripe's `cs_...` shape without pulling in a regex.
fn is_valid_session_id(s: &str) -> bool {
    (4..=255).contains(&s.len())
        && s.starts_with("cs_")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Frontpage: every category (heading links to its subpage) with its SKUs.
#[derive(askama::Template)]
#[template(path = "catalog.html")]
struct CatalogTemplate<'a> {
    title: &'a str,
    groups: Vec<CategoryGroup<'a>>,
    chrome: Chrome,
}

/// Subpage `/{category}`: a single category, for easy linking/sharing.
#[derive(askama::Template)]
#[template(path = "category.html")]
struct CategoryTemplate<'a> {
    title: &'a str,
    group: CategoryGroup<'a>,
    chrome: Chrome,
}

/// A markdown content page (Terms, Privacy, ...), served at `/p/<slug>`.
#[derive(askama::Template)]
#[template(path = "page.html")]
struct PageTemplate {
    title: String,
    body: String,
    chrome: Chrome,
}

struct CategoryGroup<'a> {
    id: &'a str,
    name: &'a str,
    description: Option<&'a str>,
    url: Option<&'a str>,
    skus: Vec<SkuView<'a>>,
}

struct SkuView<'a> {
    id: &'a str,
    display_name: &'a str,
    description: Option<&'a str>,
    url: Option<&'a str>,
    price_label: &'a str,
}

/// Build category groups in catalog order, optionally limited to one category.
/// Categories with no SKUs are dropped so an empty section never renders.
fn category_groups<'a>(cat: &'a catalog::Catalog, only: Option<&str>) -> Vec<CategoryGroup<'a>> {
    cat.categories
        .iter()
        .filter(|c| only.is_none_or(|id| c.id == id))
        .filter_map(|c| {
            let skus: Vec<SkuView> = cat
                .skus
                .iter()
                .filter(|s| s.category == c.id)
                .map(|s| SkuView {
                    id: &s.id,
                    display_name: &s.display_name,
                    description: s.description.as_deref(),
                    url: s.url.as_deref(),
                    price_label: &s.price_label,
                })
                .collect();
            if skus.is_empty() {
                return None;
            }
            Some(CategoryGroup {
                id: &c.id,
                name: &c.name,
                description: c.description.as_deref(),
                url: c.url.as_deref(),
                skus,
            })
        })
        .collect()
}

async fn catalog_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    page(&CatalogTemplate {
        title: &state.catalog.shop.title,
        groups: category_groups(&state.catalog, None),
        chrome: chrome(&state, &headers),
    })
}

async fn category_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(category): Path<String>,
) -> Response {
    let mut groups = category_groups(&state.catalog, Some(&category));
    // Filtered to one category; empty means unknown id or a category with no SKUs.
    let Some(group) = groups.pop() else {
        return (StatusCode::NOT_FOUND, "unknown category").into_response();
    };
    page(&CategoryTemplate {
        title: &state.catalog.shop.title,
        group,
        chrome: chrome(&state, &headers),
    })
}

/// Render a markdown content page from `content/<slug>.md`.
async fn page_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let Some(p) = state.catalog.page(&slug) else {
        return (StatusCode::NOT_FOUND, "unknown page").into_response();
    };
    let Some(body) = crate::content::render(&state.cfg.content_dir, &p.slug) else {
        return (StatusCode::NOT_FOUND, "page has no content").into_response();
    };
    let title = p.title.clone();
    page(&PageTemplate {
        title,
        body,
        chrome: chrome(&state, &headers),
    })
}

#[derive(Deserialize)]
struct CheckoutForm {
    sku: String,
}

async fn checkout(State(state): State<AppState>, Form(form): Form<CheckoutForm>) -> Response {
    let Some(sku) = state.catalog.by_id(&form.sku) else {
        return (StatusCode::BAD_REQUEST, "unknown product").into_response();
    };
    let success_url = format!(
        "{}/success?session_id={{CHECKOUT_SESSION_ID}}",
        state.cfg.base_url
    );
    let spec = CheckoutSpec {
        mode: Mode::Payment,
        line: Line::Price(sku.stripe_price_id.clone()),
        success_url,
        cancel_url: format!("{}/", state.cfg.base_url),
        client_reference_id: None,
        metadata: [("sku".to_string(), sku.id.clone())].into(),
        customer: None,
        customer_email: None,
        collect_tax: state.catalog.tax.mode == catalog::TaxMode::Stripe,
        // Stripe collects the email; the company becomes the license `customer`.
        custom_fields: vec![CustomField {
            key: "company".into(),
            label: "Company name".into(),
        }],
    };
    let idem_key = format!("checkout:{}", uuid::Uuid::new_v4());
    match state.stripe.create_checkout(spec, &idem_key).await {
        Ok(session) => match session.url {
            Some(url) => Redirect::to(&url).into_response(),
            None => (StatusCode::BAD_GATEWAY, "stripe returned no url").into_response(),
        },
        Err(e) => {
            tracing::error!("checkout create failed: {e:#}");
            (StatusCode::BAD_GATEWAY, "could not start checkout").into_response()
        }
    }
}

#[derive(Deserialize)]
struct SuccessQuery {
    session_id: String,
}

#[derive(askama::Template)]
#[template(path = "success.html")]
struct SuccessTemplate {
    pending: bool,
    held: bool,
    session_id: String,
    blob: String,
    product: String,
    chrome: Chrome,
}

#[derive(askama::Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    title: &'static str,
    message: &'static str,
    chrome: Chrome,
}

async fn success(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SuccessQuery>,
) -> Response {
    if !is_valid_session_id(&q.session_id) {
        return (StatusCode::BAD_REQUEST, "invalid session id").into_response();
    }
    let ch = chrome(&state, &headers);
    match fulfill::fulfill(&state, &q.session_id).await {
        Ok(fulfill::FulfillOutcome::Ready(lic)) => page(&SuccessTemplate {
            pending: false,
            held: false,
            session_id: q.session_id,
            blob: lic.blob,
            product: lic.product,
            chrome: ch,
        }),
        Ok(fulfill::FulfillOutcome::Pending) => page(&SuccessTemplate {
            pending: true,
            held: false,
            session_id: q.session_id,
            blob: String::new(),
            product: String::new(),
            chrome: ch,
        }),
        Ok(fulfill::FulfillOutcome::Held) => page(&SuccessTemplate {
            pending: false,
            held: true,
            session_id: q.session_id,
            blob: String::new(),
            product: String::new(),
            chrome: ch,
        }),
        Ok(fulfill::FulfillOutcome::LookupFailed) => (
            StatusCode::NOT_FOUND,
            page(&ErrorTemplate {
                title: "Checkout session not found",
                message: "We could not find this checkout session. \
                          Check the link you followed, or contact support.",
                chrome: ch,
            }),
        )
            .into_response(),
        Err(e) => {
            // Never log q.session_id: it is the bearer secret for this endpoint.
            tracing::error!("fulfill failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "fulfillment error").into_response()
        }
    }
}

/// Serve the same license as a downloadable file. Reuses the idempotent
/// fulfill path, so a direct hit right after payment still works.
async fn success_download(
    State(state): State<AppState>,
    Query(q): Query<SuccessQuery>,
) -> Response {
    if !is_valid_session_id(&q.session_id) {
        return (StatusCode::BAD_REQUEST, "invalid session id").into_response();
    }
    match fulfill::fulfill(&state, &q.session_id).await {
        Ok(fulfill::FulfillOutcome::Ready(lic)) => {
            // `product` is a validated slug, so it is safe to put in a header.
            let disposition = format!("attachment; filename=\"{}-license.txt\"", lic.product);
            let disposition = HeaderValue::from_str(&disposition).unwrap_or_else(|_| {
                HeaderValue::from_static("attachment; filename=\"license.txt\"")
            });
            (
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/plain; charset=utf-8"),
                    ),
                    (header::CONTENT_DISPOSITION, disposition),
                ],
                lic.blob,
            )
                .into_response()
        }
        // Not paid yet, held or unknown: the status page explains each case.
        Ok(
            fulfill::FulfillOutcome::Pending
            | fulfill::FulfillOutcome::Held
            | fulfill::FulfillOutcome::LookupFailed,
        ) => Redirect::to(&format!("/success?session_id={}", q.session_id)).into_response(),
        Err(e) => {
            // Never log q.session_id: it is the bearer secret for this endpoint.
            tracing::error!("license download failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "download error").into_response()
        }
    }
}

/// Stripe webhook. Only a bad signature is a 400 and only a retryable failure a
/// 500, since Stripe retries every non-2xx for days.
async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let Some(sig) = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing signature").into_response();
    };
    let event = match state
        .stripe
        .parse_webhook(&body, sig, &state.cfg.stripe_webhook_secret)
    {
        Ok(e) => e,
        Err(stripe_edge::Error::Signature) => {
            tracing::warn!("webhook signature rejected");
            return (StatusCode::BAD_REQUEST, "bad signature").into_response();
        }
        Err(e) => {
            tracing::error!("webhook event not processed: {e}");
            return StatusCode::OK.into_response();
        }
    };
    let event_id = event.event_id().to_owned();
    if let Err(e) = events::handle(&state, event).await {
        tracing::error!(%event_id, "webhook event failed: {e:#}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "event error").into_response();
    }
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use tower::ServiceExt;

    const CATALOG: &str = r#"
[shop]
title = "Test Shop"

[[category]]
id = "acme"
name = "Acme Corp"

[[sku]]
id = "acme-annual"
stripe_price_id = "price_x"
category = "acme"
display_name = "Acme Annual"
amount_cents = 49900
currency = "eur"
tier = "business"
term = "365d"

[[page]]
slug = "terms"
title = "Terms"
"#;

    async fn state_from(toml: &str, content_dir: PathBuf) -> AppState {
        let cfg = crate::config::AppConfig {
            stripe_api_key: "sk_test_dummy".into(),
            stripe_webhook_secret: "wh".into(),
            database_url: "sqlite::memory:".into(),
            keys_dir: "keys".into(),
            content_dir,
            base_url: "http://x".into(),
            bind_addr: "127.0.0.1:0".into(),
            trust_proxy: false,
        };
        let mut signing = std::collections::HashMap::new();
        signing.insert(
            "acme".to_string(),
            ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]),
        );
        AppState {
            cfg: std::sync::Arc::new(cfg),
            catalog: std::sync::Arc::new(crate::catalog::parse(toml).unwrap()),
            stripe: stripe_edge::Stripe::offline("signet", false),
            signing: std::sync::Arc::new(signing),
            db: crate::db::connect("sqlite::memory:").await.unwrap(),
            mail: None,
            lookups: crate::fulfill::tests::stub_lookups(),
        }
    }

    async fn test_state() -> AppState {
        state_from(CATALOG, PathBuf::from("content")).await
    }

    /// The peer-IP limiter 500s without a ConnectInfo extension, so the limited
    /// routes must carry one.
    fn with_conn(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        req
    }

    async fn send(state: AppState, req: Request<Body>) -> Response {
        router(state).oneshot(req).await.unwrap()
    }

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn index_lists_categories() {
        let state = test_state().await;
        let resp = send(state, Request::get("/").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("Acme Corp"));
    }

    #[tokio::test]
    async fn category_known_and_unknown() {
        let state = test_state().await;
        let ok = send(
            state.clone(),
            Request::get("/acme").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let missing = send(state, Request::get("/nope").body(Body::empty()).unwrap()).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn page_not_in_catalog_is_404() {
        let state = test_state().await;
        let resp = send(
            state,
            Request::get("/p/unknown").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn page_without_markdown_is_404() {
        // "terms" is in the catalog but content_dir has no terms.md.
        let state = test_state().await;
        let resp = send(state, Request::get("/p/terms").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn page_with_markdown_renders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("terms.md"), "# Terms\n\nBody text.").unwrap();
        let state = state_from(CATALOG, dir.path().to_path_buf()).await;
        let resp = send(state, Request::get("/p/terms").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("<h1>Terms</h1>"));
        assert!(body.contains("Body text."));
    }

    #[tokio::test]
    async fn checkout_unknown_sku_is_400() {
        let state = test_state().await;
        let req = with_conn(
            Request::post("/checkout")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("sku=nope"))
                .unwrap(),
        );
        let resp = send(state, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(body_string(resp).await.contains("unknown product"));
    }

    #[tokio::test]
    async fn success_rejects_invalid_session_ids() {
        let over_long = format!("cs_{}", "a".repeat(300));
        // "nope": no prefix; "cs_bad$id": illegal char; "cs_": too short.
        for bad in ["nope", "cs_bad$id", "cs_", over_long.as_str()] {
            let state = test_state().await;
            let uri = format!("/success?session_id={bad}");
            let req = with_conn(Request::get(&uri).body(Body::empty()).unwrap());
            let resp = send(state, req).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "id {bad:?}");
            assert!(body_string(resp).await.contains("invalid session id"));
        }
    }

    #[tokio::test]
    async fn success_download_rejects_invalid_session_id() {
        let state = test_state().await;
        let req = with_conn(
            Request::get("/success/download?session_id=nope")
                .body(Body::empty())
                .unwrap(),
        );
        let resp = send(state, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn webhook_missing_and_bad_signature() {
        let state = test_state().await;
        let missing = send(
            state.clone(),
            Request::post("/webhook").body(Body::from("{}")).unwrap(),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        assert!(body_string(missing).await.contains("missing signature"));

        let bad = send(
            state,
            Request::post("/webhook")
                .header("stripe-signature", "t=1,v1=deadbeef")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        assert!(body_string(bad).await.contains("bad signature"));
    }

    /// A `checkout.session.completed` webhook for `session`, which the stub lookup also returns.
    fn completed_webhook(session: &stripe_edge::SessionInfo) -> Request<Body> {
        crate::fulfill::tests::stub_session(session);
        signed_webhook(stripe_edge::test_util::session_completed_json(session))
    }

    fn signed_webhook(payload: String) -> Request<Body> {
        let sig = stripe_edge::test_util::sign(&payload, "wh", chrono::Utc::now().timestamp());
        Request::post("/webhook")
            .header("stripe-signature", sig)
            .body(Body::from(payload))
            .unwrap()
    }

    async fn count(state: &AppState, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&state.db)
            .await
            .unwrap()
    }

    async fn outbox_kinds(state: &AppState) -> Vec<String> {
        sqlx::query_scalar("SELECT kind FROM mail_outbox ORDER BY id")
            .fetch_all(&state.db)
            .await
            .unwrap()
    }

    fn mailed(state: AppState) -> AppState {
        crate::outbox::tests::with_mail(state, &crate::outbox::tests::TestMailer::default())
    }

    #[tokio::test]
    async fn webhook_completed_session_mints_once() {
        let state = mailed(test_state().await);
        let mut session = crate::fulfill::tests::paid_session("cs_test_wh", "acme-annual");
        session.payment_intent_id = Some("pi_clean".into());
        let resp = send(state.clone(), completed_webhook(&session)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let stored = crate::db::get_by_session(&state.db, "cs_test_wh")
            .await
            .unwrap()
            .expect("license stored");
        assert_eq!(stored.customer, "Acme GmbH");
        assert_eq!(stored.sku, "acme-annual");

        let resp = send(state.clone(), completed_webhook(&session)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(count(&state, "licenses").await, 1);
        let again = crate::db::get_by_session(&state.db, "cs_test_wh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.blob, stored.blob);
        assert_eq!(count(&state, "holds").await, 0);
        assert_eq!(outbox_kinds(&state).await, ["license", "sale_notice"]);
    }

    /// The webhook and the success page fulfil the same session at once on a file DB.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn webhook_and_success_race_mint_one_licence_and_two_mails() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("shop.db").display());
        let mut state = mailed(test_state().await);
        state.db = crate::db::connect(&url).await.unwrap();
        for i in 0..8 {
            let id = format!("cs_test_race_{i}");
            let session = crate::fulfill::tests::paid_session(&id, "acme-annual");
            let hook = tokio::spawn(send(state.clone(), completed_webhook(&session)));
            let st = state.clone();
            let page =
                tokio::spawn(
                    async move { crate::fulfill::fulfill_paid_session(&st, &session).await },
                );
            assert_eq!(hook.await.unwrap().status(), StatusCode::OK);
            let crate::fulfill::FulfillOutcome::Ready(direct) = page.await.unwrap().unwrap() else {
                panic!("expected Ready");
            };
            let stored = crate::db::get_by_session(&state.db, &id)
                .await
                .unwrap()
                .expect("license stored");
            assert_eq!(direct.blob, stored.blob);
            let kinds: Vec<String> = sqlx::query_scalar(
                "SELECT kind FROM mail_outbox WHERE json_extract(payload, '$.session_id') = ?
                 ORDER BY id",
            )
            .bind(&id)
            .fetch_all(&state.db)
            .await
            .unwrap();
            assert_eq!(kinds, ["license", "sale_notice"]);
        }
        assert_eq!(count(&state, "licenses").await, 8);
        assert_eq!(count(&state, "mail_outbox").await, 16);
    }

    #[tokio::test]
    async fn webhook_missing_sku_is_held_and_acknowledged() {
        let state = mailed(test_state().await);
        let mut session = crate::fulfill::tests::paid_session("cs_test_nosku", "acme-annual");
        session.metadata.clear();
        for _ in 0..2 {
            let resp = send(state.clone(), completed_webhook(&session)).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(count(&state, "licenses").await, 0);
        assert_eq!(count(&state, "holds").await, 1);
        let hold = crate::db::open_hold(&state.db, "cs_test_nosku")
            .await
            .unwrap()
            .expect("hold");
        assert_eq!(hold.reason, "missing_sku");
        assert_eq!(outbox_kinds(&state).await, ["hold_notice"]);

        // Signed but not a Stripe event.
        let resp = send(state, signed_webhook("{}".into())).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn webhook_other_mode_event_is_ignored_both_ways() {
        for account_live in [false, true] {
            let mut state = test_state().await;
            state.stripe = stripe_edge::Stripe::offline("signet", account_live);
            let mut session = crate::fulfill::tests::paid_session("cs_test_mode", "acme-annual");
            session.livemode = !account_live;
            let payload = stripe_edge::test_util::session_completed_json(&session);
            let resp = send(state.clone(), signed_webhook(payload)).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(count(&state, "licenses").await, 0);
            assert_eq!(count(&state, "holds").await, 0);
        }
    }

    #[tokio::test]
    async fn success_shows_held_and_download_redirects() {
        let state = test_state().await;
        crate::db::insert_hold(&state.db, "cs_test_held", "missing_sku", None)
            .await
            .unwrap();
        let req = with_conn(
            Request::get("/success?session_id=cs_test_held")
                .body(Body::empty())
                .unwrap(),
        );
        let resp = send(state.clone(), req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("being checked"), "{body}");
        assert!(!body.contains("still processing"));

        let req = with_conn(
            Request::get("/success/download?session_id=cs_test_held")
                .body(Body::empty())
                .unwrap(),
        );
        let resp = send(state, req).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers()[header::LOCATION],
            "/success?session_id=cs_test_held"
        );
    }

    #[tokio::test]
    async fn held_sale_is_released_by_the_operator() {
        let state = mailed(test_state().await);
        let mut session = crate::fulfill::tests::paid_session("cs_test_release", "acme-annual");
        session.amount_total = Some(100);
        let resp = send(state.clone(), completed_webhook(&session)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let hold = crate::db::open_hold(&state.db, "cs_test_release")
            .await
            .unwrap()
            .expect("hold");
        assert_eq!(hold.reason, "mismatch:amount 100 is below 49900");

        // A session lookup would now find nothing, so the held page proves none was made.
        crate::fulfill::tests::forget_session(&session.id);
        let page = || {
            with_conn(
                Request::get("/success?session_id=cs_test_release")
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let body = body_string(send(state.clone(), page()).await).await;
        assert!(body.contains("being checked"), "{body}");

        let rec = crate::cli::release_paid(&state, &session).await.unwrap();
        assert!(crate::db::open_hold(&state.db, "cs_test_release")
            .await
            .unwrap()
            .is_none());
        let stored = crate::db::get_by_session(&state.db, "cs_test_release")
            .await
            .unwrap()
            .expect("licence");
        assert_eq!(stored.blob, rec.blob);
        assert_eq!(stored.amount_total, Some(100));

        let resp = send(state.clone(), page()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(
            body.contains("Thank you") && body.contains(&rec.blob),
            "{body}"
        );

        let err = crate::cli::release_paid(&state, &session)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no open hold for cs_test_release");
        assert_eq!(count(&state, "licenses").await, 1);
        assert_eq!(
            outbox_kinds(&state).await,
            ["hold_notice", "license", "sale_notice"]
        );
    }

    #[tokio::test]
    async fn webhook_store_failure_is_retried() {
        let state = test_state().await;
        state.db.close().await;
        let session = crate::fulfill::tests::paid_session("cs_test_db", "acme-annual");
        let resp = send(state, completed_webhook(&session)).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn webhook_mints_from_the_fetched_session_not_the_payload() {
        let state = mailed(test_state().await);
        let mut fetched = crate::fulfill::tests::paid_session("cs_test_unpaid", "acme-annual");
        fetched.paid = false;
        crate::fulfill::tests::stub_session(&fetched);
        let mut payload = fetched.clone();
        payload.paid = true;
        let json = stripe_edge::test_util::session_completed_json(&payload);
        let resp = send(state.clone(), signed_webhook(json)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(count(&state, "licenses").await, 0);
        assert_eq!(count(&state, "holds").await, 0);
    }

    #[tokio::test]
    async fn webhook_holds_a_reversed_or_unreadable_payment() {
        for (id, pi, reason, kinds) in [
            (
                "cs_test_refunded",
                "pi_refunded",
                "refund:ch_refunded",
                &[][..],
            ),
            (
                "cs_test_disputed",
                "pi_disputed",
                "dispute:charge:ch_disputed",
                &[],
            ),
            (
                "cs_test_lookup",
                "pi_permanent",
                "lookup_failed",
                &["hold_notice"],
            ),
            (
                "cs_test_nocharge",
                "pi_missing",
                "lookup_failed",
                &["hold_notice"],
            ),
        ] {
            let state = mailed(test_state().await);
            let mut session = crate::fulfill::tests::paid_session(id, "acme-annual");
            session.payment_intent_id = Some(pi.into());
            let resp = send(state.clone(), completed_webhook(&session)).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(count(&state, "licenses").await, 0);
            let hold = crate::db::open_hold(&state.db, id)
                .await
                .unwrap()
                .expect("hold");
            assert_eq!(hold.reason, reason);
            assert_eq!(outbox_kinds(&state).await, kinds);
        }
    }

    #[tokio::test]
    async fn dispute_with_an_unreadable_charge_is_held_and_acknowledged() {
        let state = mailed(test_state().await);
        let payload = include_str!("../tests/fixtures/dispute_created.json");
        let resp = send(state.clone(), signed_webhook(payload.into())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let hold = crate::db::open_hold(&state.db, "charge:ch_3USz0qBXbQNWZa4n0lqqKPUG")
            .await
            .unwrap()
            .expect("hold");
        assert_eq!(hold.reason, "lookup_failed:permanent");
        assert_eq!(outbox_kinds(&state).await, ["hold_notice"]);
    }

    #[tokio::test]
    async fn webhook_holds_a_session_stripe_cannot_return() {
        // Unregistered in the stub: not found; `cs_test_permanent`: a permanent error.
        for id in ["cs_test_vanished", "cs_test_permanent"] {
            let state = mailed(test_state().await);
            let session = crate::fulfill::tests::paid_session(id, "acme-annual");
            let json = stripe_edge::test_util::session_completed_json(&session);
            let resp = send(state.clone(), signed_webhook(json)).await;
            assert_eq!(resp.status(), StatusCode::OK, "{id}");
            assert_eq!(count(&state, "licenses").await, 0);
            let hold = crate::db::open_hold(&state.db, id)
                .await
                .unwrap()
                .expect("hold");
            assert_eq!(hold.reason, "lookup_failed:session");
            assert_eq!(outbox_kinds(&state).await, ["hold_notice"]);
        }
    }

    #[tokio::test]
    async fn webhook_transient_charge_lookup_is_retried_and_writes_nothing() {
        let state = mailed(test_state().await);
        let mut session = crate::fulfill::tests::paid_session("cs_test_transient", "acme-annual");
        session.payment_intent_id = Some("pi_transient".into());
        let resp = send(state.clone(), completed_webhook(&session)).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        for table in ["licenses", "holds", "mail_outbox"] {
            assert_eq!(count(&state, table).await, 0, "{table}");
        }
    }

    #[tokio::test]
    async fn success_for_a_disputed_payment_shows_the_held_page() {
        let state = test_state().await;
        let mut session =
            crate::fulfill::tests::paid_session("cs_test_success_disputed", "acme-annual");
        session.payment_intent_id = Some("pi_disputed".into());
        crate::fulfill::tests::stub_session(&session);
        let req = with_conn(
            Request::get("/success?session_id=cs_test_success_disputed")
                .body(Body::empty())
                .unwrap(),
        );
        let resp = send(state.clone(), req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("being checked"), "{body}");
        assert_eq!(count(&state, "licenses").await, 0);
        let hold = crate::db::open_hold(&state.db, &session.id)
            .await
            .unwrap()
            .expect("hold");
        assert_eq!(hold.reason, "dispute:charge:ch_disputed");
    }

    #[tokio::test]
    async fn checkout_stripe_unreachable_is_502() {
        let state = test_state().await;
        let req = with_conn(
            Request::post("/checkout")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("sku=acme-annual"))
                .unwrap(),
        );
        let resp = send(state, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn security_headers_present() {
        let state = test_state().await;
        let resp = send(state, Request::get("/").body(Body::empty()).unwrap()).await;
        let h = resp.headers();
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("form-action 'self' https://checkout.stripe.com"));
        assert!(csp.contains("frame-ancestors 'none'"));
    }

    #[tokio::test]
    async fn csp_reflects_analytics_config() {
        let with_analytics =
            format!("{CATALOG}\n[analytics]\nsrc = \"https://plausible.example/js/script.js\"\n");
        let state = state_from(&with_analytics, PathBuf::from("content")).await;
        let resp = send(state, Request::get("/").body(Body::empty()).unwrap()).await;
        let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("script-src 'self' https://plausible.example"));
        assert!(csp.contains("connect-src 'self' https://plausible.example"));

        let state = test_state().await;
        let resp = send(state, Request::get("/").body(Body::empty()).unwrap()).await;
        let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(!csp.contains("plausible"));
    }

    #[tokio::test]
    async fn theme_cookie_selects_palette() {
        let state = test_state().await;
        let resp = send(
            state,
            Request::get("/")
                .header("cookie", "license_theme=dark")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(body_string(resp).await.contains(r#"data-theme="dark""#));

        let state = test_state().await;
        let resp = send(
            state,
            Request::get("/")
                .header("cookie", "license_theme=bogus")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(body_string(resp).await.contains(r#"data-theme="system""#));
    }
}
