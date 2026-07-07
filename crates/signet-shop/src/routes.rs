use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use stripe_webhook::{EventObject, Webhook};

use crate::render::page;
use crate::state::AppState;
use crate::{catalog, fulfill, payments};

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
    let cancel_url = format!("{}/", state.cfg.base_url);
    match payments::create_checkout_session(
        &state.stripe,
        &sku.stripe_price_id,
        &sku.id,
        &success_url,
        &cancel_url,
    )
    .await
    {
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
    session_id: String,
    blob: String,
    product: String,
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
            session_id: q.session_id,
            blob: lic.blob,
            product: lic.product,
            chrome: ch,
        }),
        Ok(fulfill::FulfillOutcome::Pending) => page(&SuccessTemplate {
            pending: true,
            session_id: q.session_id,
            blob: String::new(),
            product: String::new(),
            chrome: ch,
        }),
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
        // Not paid yet: send them back to the status page to refresh.
        Ok(fulfill::FulfillOutcome::Pending) => {
            Redirect::to(&format!("/success?session_id={}", q.session_id)).into_response()
        }
        Err(e) => {
            // Never log q.session_id: it is the bearer secret for this endpoint.
            tracing::error!("license download failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "download error").into_response()
        }
    }
}

/// Stripe webhook. `stripe_webhook::Webhook::construct_event` verifies the
/// signature over the raw body (replacing our hand-rolled HMAC). We fulfill on
/// both the instant `completed` and the delayed `async_payment_succeeded`
/// events; dynamic payment methods can enable async rails from the Dashboard.
async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let Some(sig) = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing signature").into_response();
    };
    let event = match Webhook::construct_event(&body, sig, &state.cfg.stripe_webhook_secret) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("webhook signature/parse rejected: {e:?}");
            return (StatusCode::BAD_REQUEST, "bad signature").into_response();
        }
    };

    let session = match event.data.object {
        EventObject::CheckoutSessionCompleted(s)
        | EventObject::CheckoutSessionAsyncPaymentSucceeded(s) => Some(s),
        _ => None,
    };
    if let Some(s) = session {
        let info = payments::SessionInfo::from_session(&s);
        if let Err(e) = fulfill::fulfill_paid_session(&state, &info).await {
            tracing::error!("webhook fulfill failed: {e:#}");
            // Non-2xx so Stripe retries the delivery.
            return (StatusCode::INTERNAL_SERVER_ERROR, "fulfill error").into_response();
        }
    }
    StatusCode::OK.into_response()
}
