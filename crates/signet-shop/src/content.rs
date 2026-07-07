//! Markdown content pages, read from `content/<slug>.md` at request time so
//! edits show up without a restart. Author-trusted like the rest of the
//! per-site config, and the strict CSP neutralizes any injected script.

use std::path::Path;

use pulldown_cmark::{html, Options, Parser};

/// A slug is a validated path component; reject anything that could escape the
/// content directory, as defence in depth over the catalog's own validation.
fn safe_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Render `content/<slug>.md` to HTML. `None` if the slug is unsafe or the file
/// is missing.
pub fn render(content_dir: &Path, slug: &str) -> Option<String> {
    if !safe_slug(slug) {
        return None;
    }
    let md = std::fs::read_to_string(content_dir.join(format!("{slug}.md"))).ok()?;
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(&md, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    Some(out)
}

/// Whether `content/<slug>.md` exists, so the footer can hide links to pages
/// that have no content yet.
pub fn exists(content_dir: &Path, slug: &str) -> bool {
    safe_slug(slug) && content_dir.join(format!("{slug}.md")).is_file()
}
