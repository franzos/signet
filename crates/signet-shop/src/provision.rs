use anyhow::{Context, Result};
use stripe_edge::{PriceChange, PriceSpec, Stripe};

use crate::catalog::{self, Sku, TaxMode};

/// `signet-shop provision-stripe`: reconcile every SKU in `catalog.toml` with
/// Stripe. Each SKU gets a Product (`signet_<sku id>`, or the product of the
/// price the catalog already names) and a one-time Price, and the resolved
/// `stripe_price_id` is written back into the file, preserving comments. It is
/// idempotent: an unchanged catalog produces no Stripe calls that mutate state
/// and no file write.
pub async fn run() -> Result<()> {
    let api_key = std::env::var("STRIPE_API_KEY")
        .or_else(|_| std::env::var("STRIPE_SECRET_KEY"))
        .context("set STRIPE_API_KEY (or STRIPE_SECRET_KEY) to provision Stripe")?;
    let stripe = Stripe::connect(&api_key, crate::STRIPE_SERVICE)
        .await
        .context("connect to Stripe")?;

    let path = catalog::default_path();
    let mut text = std::fs::read_to_string(&path)
        .with_context(|| format!("read catalog {}", path.display()))?;
    let cat = catalog::parse(&text).with_context(|| format!("parse catalog {}", path.display()))?;

    println!("Reconciling {} SKU(s) against Stripe.\n", cat.skus.len());
    println!("{:<32} {:<8} price id", "sku", "status");

    let tax_inclusive = cat.tax.mode == TaxMode::Stripe;
    let mut changed = false;
    for sku in &cat.skus {
        let change = stripe
            .ensure_price(price_spec(sku, tax_inclusive))
            .await
            .with_context(|| format!("reconcile {}", sku.id))?;
        let new = match change {
            PriceChange::Unchanged => {
                println!("{:<32} {:<8} {}", sku.id, "ok", sku.stripe_price_id);
                continue;
            }
            PriceChange::Set(id) => {
                println!("{:<32} {:<8} {}", sku.id, "set", id);
                id
            }
            PriceChange::Superseded { old, new } => {
                println!("{:<32} {:<8} {} (was {})", sku.id, "updated", new, old);
                new
            }
        };
        text = stripe_edge::set_toml_field(&text, "sku", &sku.id, "stripe_price_id", &new)
            .with_context(|| format!("write price id for {}", sku.id))?;
        changed = true;
    }

    if changed {
        std::fs::write(&path, text).with_context(|| format!("write catalog {}", path.display()))?;
        println!("\nWrote resolved price ids to {}.", path.display());
    } else {
        println!("\nNo changes: {} already matches Stripe.", path.display());
    }
    Ok(())
}

fn price_spec(sku: &Sku, tax_inclusive: bool) -> PriceSpec {
    PriceSpec {
        product_key: sku.id.clone(),
        product_name: sku.display_name.clone(),
        product_adopt: None,
        unit_amount: sku.amount_cents,
        currency: sku.currency.clone(),
        tax_inclusive,
        recurring: None,
        current: Some(sku.stripe_price_id.clone()).filter(|id| !id.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn price_id_write_back_matches_the_example_catalog() {
        let src = include_str!("../../../catalog.example.toml");
        let out = stripe_edge::set_toml_field(
            src,
            "sku",
            "acme-business-annual",
            "stripe_price_id",
            "price_new",
        )
        .unwrap();
        // Only the first SKU's empty id changes; comments and alignment stay.
        let want = src.replacen(
            r#"stripe_price_id = """#,
            r#"stripe_price_id = "price_new""#,
            1,
        );
        assert_eq!(out, want);
    }

    #[test]
    fn price_spec_adopts_the_catalog_price() {
        let cat = crate::catalog::parse(include_str!("../../../catalog.example.toml")).unwrap();
        let mut sku = cat.skus[0].clone();
        let spec = super::price_spec(&sku, true);
        assert_eq!(spec.current, None);
        assert_eq!(spec.product_key, sku.id);
        assert_eq!(spec.currency, "eur");
        assert!(spec.tax_inclusive && spec.recurring.is_none());
        sku.stripe_price_id = "price_live".into();
        assert_eq!(
            super::price_spec(&sku, false).current.as_deref(),
            Some("price_live")
        );
    }
}
