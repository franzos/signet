use std::hash::{Hash, Hasher};

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

static STATIC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/static");

pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches("/static/");
    let Some(f) = STATIC_DIR.get_file(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let bytes = f.contents();

    // Assets are embedded in the binary, so a content hash is a stable ETag that
    // changes exactly when the asset does (i.e. on rebuild). `no-cache` makes the
    // browser revalidate every time and take a cheap 304 when unchanged, which
    // avoids serving stale CSS/JS after an edit while still skipping re-downloads.
    let etag = etag_for(bytes);
    let etag_val = HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"static\""));

    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm == etag)
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag_val)]).into_response();
    }

    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(mime_for(path)),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (header::ETAG, etag_val),
        ],
        bytes,
    )
        .into_response()
}

fn etag_for(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:x}\"", h.finish())
}

fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}
