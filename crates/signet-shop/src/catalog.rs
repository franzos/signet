//! Runtime catalog: products/plans are defined in a per-site `catalog.toml`
//! (path via `CATALOG_PATH`, default `./catalog.toml`), never baked into the
//! binary. Parsed and validated into owned `Sku`s at startup.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use stripe_types::Currency;

/// License term, converted to an `expires_at` at issue time.
#[derive(Debug, Clone, Copy)]
pub enum Term {
    Lifetime,
    Days(i64),
}

/// A product line (e.g. Acme, Globex). Its `id` selects the signing key
/// (`keys/<id>/private.bin`) and is stamped into the license as the `product`
/// claim; SKUs reference it via their `category` field.
#[derive(Debug, Clone)]
pub struct Category {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Sku {
    pub id: String,
    pub stripe_price_id: String,
    pub category: String,
    pub display_name: String,
    pub description: Option<String>,
    pub url: Option<String>,
    pub price_label: String,
    pub amount_cents: i64,
    pub currency: Currency,
    pub tier: String,
    pub features: Vec<String>,
    pub max_orgs: Option<u32>,
    pub max_seats: Option<u32>,
    pub term: Term,
}

impl Sku {
    pub fn to_issue_params(
        &self,
        customer: String,
        email: String,
        now_unix: i64,
        note: String,
    ) -> signetlib::IssueParams {
        let expires_at = match self.term {
            Term::Lifetime => None,
            Term::Days(d) => Some(now_unix + d * 86_400),
        };
        signetlib::IssueParams {
            product: self.category.clone(),
            customer,
            email,
            tier: self.tier.clone(),
            expires_at,
            features: self.features.clone(),
            max_orgs: self.max_orgs,
            max_seats: self.max_seats,
            note,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShopConfig {
    pub title: String,
    /// Footer notice about payment data handling. `None` uses a default; set it
    /// to an empty string to hide the notice entirely.
    pub payment_notice: Option<String>,
}

impl Default for ShopConfig {
    fn default() -> Self {
        Self {
            title: "License Shop".into(),
            payment_notice: None,
        }
    }
}

/// An extra footer link (title + destination), configured per site.
#[derive(Debug, Clone)]
pub struct FooterLink {
    pub title: String,
    pub url: String,
}

/// A markdown content page: `content/<slug>.md`, served at `/p/<slug>`.
#[derive(Debug, Clone)]
pub struct Page {
    pub slug: String,
    pub title: String,
    /// Whether to link this page from the footer.
    pub footer: bool,
}

/// Optional analytics snippet. Rendered into `<head>`; its origin is added to
/// the CSP so the strict policy still allows exactly this host.
#[derive(Debug, Clone)]
pub struct Analytics {
    pub src: String,
    pub entity: Option<String>,
    pub module: bool,
}

pub struct Catalog {
    pub shop: ShopConfig,
    pub categories: Vec<Category>,
    pub skus: Vec<Sku>,
    pub footer_links: Vec<FooterLink>,
    pub pages: Vec<Page>,
    pub analytics: Option<Analytics>,
}

impl Catalog {
    pub fn by_id(&self, id: &str) -> Option<&Sku> {
        self.skus.iter().find(|s| s.id == id)
    }

    pub fn page(&self, slug: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.slug == slug)
    }

    /// Distinct categories referenced by the catalog; each needs a signing key.
    pub fn category_ids(&self) -> BTreeSet<&str> {
        self.categories.iter().map(|c| c.id.as_str()).collect()
    }

    /// Every purchasable SKU must have a Stripe price id. Called at serve time, not
    /// parse time, so `provision-stripe` can still load an un-provisioned catalog.
    pub fn ensure_provisioned(&self) -> anyhow::Result<()> {
        let missing: Vec<&str> = self
            .skus
            .iter()
            .filter(|s| s.stripe_price_id.is_empty())
            .map(|s| s.id.as_str())
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "SKU(s) missing stripe_price_id: {} (run `signet-shop provision-stripe`)",
                missing.join(", ")
            );
        }
        Ok(())
    }
}

/// `CATALOG_PATH`, or `./catalog.toml`.
pub fn default_path() -> PathBuf {
    std::env::var("CATALOG_PATH")
        .unwrap_or_else(|_| "./catalog.toml".into())
        .into()
}

pub fn load(path: &Path) -> Result<Catalog> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read catalog {}", path.display()))?;
    parse(&text).with_context(|| format!("parse catalog {}", path.display()))
}

#[derive(Deserialize)]
struct CatalogFile {
    #[serde(default)]
    shop: ShopConfig,
    #[serde(default)]
    category: Vec<CategoryConfig>,
    #[serde(default)]
    sku: Vec<SkuConfig>,
    #[serde(default)]
    footer_link: Vec<FooterLinkConfig>,
    #[serde(default)]
    page: Vec<PageConfig>,
    #[serde(default)]
    analytics: Option<AnalyticsConfig>,
}

#[derive(Deserialize)]
struct FooterLinkConfig {
    title: String,
    url: String,
}

#[derive(Deserialize)]
struct PageConfig {
    slug: String,
    title: String,
    #[serde(default)]
    footer: bool,
}

#[derive(Deserialize)]
struct AnalyticsConfig {
    src: String,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    module: bool,
}

#[derive(Deserialize)]
struct CategoryConfig {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct SkuConfig {
    id: String,
    #[serde(default)]
    stripe_price_id: String,
    category: String,
    display_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    price_label: String,
    amount_cents: i64,
    currency: String,
    tier: String,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    max_orgs: Option<u32>,
    #[serde(default)]
    max_seats: Option<u32>,
    term: String,
}

pub fn parse(text: &str) -> Result<Catalog> {
    let file: CatalogFile = toml::from_str(text).context("invalid TOML")?;

    let mut categories = Vec::with_capacity(file.category.len());
    let mut cat_ids = BTreeSet::new();
    for cc in file.category {
        if cc.id.is_empty() {
            bail!("a [[category]] has an empty id");
        }
        if !cat_ids.insert(cc.id.clone()) {
            bail!("duplicate category id {:?}", cc.id);
        }
        if cc.name.is_empty() {
            bail!("category {:?} has an empty name", cc.id);
        }
        // The id is a URL path segment (its subpage is `/<id>`), so keep it a
        // slug and off the reserved route names.
        if !cc
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            bail!(
                "category id {:?} must be a slug (lowercase letters, digits, hyphens)",
                cc.id
            );
        }
        if matches!(
            cc.id.as_str(),
            "checkout" | "success" | "webhook" | "static"
        ) {
            bail!(
                "category id {:?} is reserved (collides with a route)",
                cc.id
            );
        }
        categories.push(Category {
            id: cc.id,
            name: cc.name,
            description: cc.description,
            url: cc.url,
        });
    }

    let mut skus = Vec::with_capacity(file.sku.len());
    let mut seen = BTreeSet::new();
    for sc in file.sku {
        if sc.id.is_empty() {
            bail!("a [[sku]] has an empty id");
        }
        if !seen.insert(sc.id.clone()) {
            bail!("duplicate sku id {:?}", sc.id);
        }
        if !cat_ids.contains(sc.category.as_str()) {
            bail!(
                "sku {:?}: category {:?} is not defined by any [[category]]",
                sc.id,
                sc.category
            );
        }
        // from_str is infallible: unknown codes become Currency::Unknown, so
        // validate explicitly rather than trusting the Result.
        let currency = Currency::from_str(&sc.currency).unwrap();
        if matches!(currency, Currency::Unknown(_)) {
            bail!(
                "sku {:?}: unknown currency {:?} (use a lowercase ISO code like \"eur\")",
                sc.id,
                sc.currency
            );
        }
        let term = parse_term(&sc.term).with_context(|| format!("sku {:?} term", sc.id))?;
        skus.push(Sku {
            id: sc.id,
            stripe_price_id: sc.stripe_price_id,
            category: sc.category,
            display_name: sc.display_name,
            description: sc.description,
            url: sc.url,
            price_label: sc.price_label,
            amount_cents: sc.amount_cents,
            currency,
            tier: sc.tier,
            features: sc.features,
            max_orgs: sc.max_orgs,
            max_seats: sc.max_seats,
            term,
        });
    }

    let mut pages = Vec::with_capacity(file.page.len());
    let mut page_slugs = BTreeSet::new();
    for pc in file.page {
        if !pc
            .slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || pc.slug.is_empty()
        {
            bail!(
                "page slug {:?} must be a slug (lowercase letters, digits, hyphens)",
                pc.slug
            );
        }
        if !page_slugs.insert(pc.slug.clone()) {
            bail!("duplicate page slug {:?}", pc.slug);
        }
        if pc.title.is_empty() {
            bail!("page {:?} has an empty title", pc.slug);
        }
        pages.push(Page {
            slug: pc.slug,
            title: pc.title,
            footer: pc.footer,
        });
    }

    let mut footer_links = Vec::with_capacity(file.footer_link.len());
    for fl in file.footer_link {
        if fl.title.is_empty() || fl.url.is_empty() {
            bail!("a [[footer_link]] needs a non-empty title and url");
        }
        footer_links.push(FooterLink {
            title: fl.title,
            url: fl.url,
        });
    }

    let analytics = match file.analytics {
        Some(a) => {
            // https only, so a single https origin can be added to the CSP.
            if !a.src.starts_with("https://") {
                bail!("[analytics] src must be an https URL, got {:?}", a.src);
            }
            Some(Analytics {
                src: a.src,
                entity: a.entity,
                module: a.module,
            })
        }
        None => None,
    };

    Ok(Catalog {
        shop: file.shop,
        categories,
        skus,
        footer_links,
        pages,
        analytics,
    })
}

fn parse_term(s: &str) -> Result<Term> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("lifetime") {
        return Ok(Term::Lifetime);
    }
    let num = t.strip_suffix('d').unwrap_or(t);
    let days: i64 = num
        .parse()
        .map_err(|_| anyhow!("term must be \"lifetime\" or \"<days>d\", got {t:?}"))?;
    if days <= 0 {
        bail!("term days must be positive, got {days}");
    }
    Ok(Term::Days(days))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[shop]
title = "Test Shop"

[[category]]
id = "acme"
name = "Acme"
description = "IAM"
url = "https://acme.example"

[[sku]]
id = "acme-annual"
category = "acme"
display_name = "Acme (annual)"
description = "Annual plan"
url = "https://acme.example/annual"
price_label = "EUR 499 / year"
amount_cents = 49900
currency = "eur"
tier = "business"
features = ["orgs", "saml"]
max_orgs = 50
term = "365d"

[[sku]]
id = "acme-lifetime"
category = "acme"
display_name = "Acme (lifetime)"
amount_cents = 149900
currency = "eur"
tier = "business"
term = "lifetime"
"#;

    #[test]
    fn parses_and_converts() {
        let cat = parse(SAMPLE).unwrap();
        assert_eq!(cat.shop.title, "Test Shop");
        assert_eq!(cat.skus.len(), 2);
        assert_eq!(
            cat.category_ids().into_iter().collect::<Vec<_>>(),
            vec!["acme"]
        );

        let acme = cat.categories.iter().find(|c| c.id == "acme").unwrap();
        assert_eq!(acme.name, "Acme");
        assert_eq!(acme.description.as_deref(), Some("IAM"));
        assert_eq!(acme.url.as_deref(), Some("https://acme.example"));

        let annual = cat.by_id("acme-annual").unwrap();
        assert_eq!(annual.currency, Currency::EUR);
        assert_eq!(annual.description.as_deref(), Some("Annual plan"));
        let p = annual.to_issue_params("Acme".into(), "a@b.c".into(), 1_000_000, "web".into());
        assert_eq!(p.product, "acme");
        assert_eq!(p.features, vec!["orgs", "saml"]);
        assert_eq!(p.max_orgs, Some(50));
        assert_eq!(p.expires_at, Some(1_000_000 + 365 * 86_400));

        // Optional fields default to None when omitted.
        let life = cat.by_id("acme-lifetime").unwrap();
        assert_eq!(life.description, None);
        let lp = life.to_issue_params("Acme".into(), "a@b.c".into(), 0, String::new());
        assert_eq!(lp.expires_at, None);
    }

    #[test]
    fn default_title_when_shop_omitted() {
        let cat = parse(
            r#"
[[category]]
id = "p"
name = "P"

[[sku]]
id = "x"
category = "p"
display_name = "X"
amount_cents = 100
currency = "eur"
tier = "t"
term = "lifetime"
"#,
        )
        .unwrap();
        assert_eq!(cat.shop.title, "License Shop");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let dup = r#"
[[category]]
id = "p"
name = "P"

[[sku]]
id = "x"
category = "p"
display_name = "X"
amount_cents = 100
currency = "eur"
tier = "t"
term = "lifetime"

[[sku]]
id = "x"
category = "p"
display_name = "X2"
amount_cents = 100
currency = "eur"
tier = "t"
term = "lifetime"
"#;
        assert!(parse(dup).is_err());
    }

    #[test]
    fn rejects_sku_with_undefined_category() {
        let bad = r#"
[[category]]
id = "acme"
name = "Acme"

[[sku]]
id = "x"
category = "globex"
display_name = "X"
amount_cents = 100
currency = "eur"
tier = "t"
term = "lifetime"
"#;
        assert!(parse(bad).is_err());
    }

    #[test]
    fn rejects_reserved_or_non_slug_category_id() {
        let reserved = r#"
[[category]]
id = "success"
name = "Nope"
"#;
        assert!(parse(reserved).is_err());
        let non_slug = r#"
[[category]]
id = "Acme Cloud"
name = "Nope"
"#;
        assert!(parse(non_slug).is_err());
    }

    #[test]
    fn rejects_duplicate_category_ids() {
        let bad = r#"
[[category]]
id = "acme"
name = "Acme"

[[category]]
id = "acme"
name = "Acme 2"
"#;
        assert!(parse(bad).is_err());
    }

    #[test]
    fn rejects_unknown_currency() {
        let bad = r#"
[[category]]
id = "p"
name = "P"

[[sku]]
id = "x"
category = "p"
display_name = "X"
amount_cents = 100
currency = "zzz"
tier = "t"
term = "lifetime"
"#;
        assert!(parse(bad).is_err());
    }

    #[test]
    fn ensure_provisioned_requires_price_ids() {
        let unprovisioned = r#"
[[category]]
id = "p"
name = "P"

[[sku]]
id = "x"
category = "p"
display_name = "X"
amount_cents = 100
currency = "eur"
tier = "t"
term = "lifetime"
"#;
        let cat = parse(unprovisioned).unwrap();
        assert!(cat.ensure_provisioned().is_err());

        let provisioned = r#"
[[category]]
id = "p"
name = "P"

[[sku]]
id = "x"
stripe_price_id = "price_x"
category = "p"
display_name = "X"
amount_cents = 100
currency = "eur"
tier = "t"
term = "lifetime"
"#;
        let cat = parse(provisioned).unwrap();
        assert!(cat.ensure_provisioned().is_ok());
    }

    #[test]
    fn rejects_bad_term() {
        let bad = r#"
[[category]]
id = "p"
name = "P"

[[sku]]
id = "x"
category = "p"
display_name = "X"
amount_cents = 100
currency = "eur"
tier = "t"
term = "banana"
"#;
        assert!(parse(bad).is_err());
    }
}
