use anyhow::{bail, Context, Result};
use stripe_edge::{PriceAction, PriceChange, PriceSpec, ProductAction, Stripe};

use crate::catalog::{self, Sku, TaxMode};

/// `signet-shop provision-stripe [--dry-run]`: reconcile every SKU in
/// `catalog.toml` with Stripe. Each SKU gets a Product (`signet_<sku id>`, or the
/// product of the price the catalog already names) and a one-time Price, and the
/// resolved `stripe_price_id` is written back into the file, preserving comments.
/// It is idempotent: an unchanged catalog produces no Stripe calls that mutate
/// state and no file write. `--dry-run` reports the action per SKU using reads only.
pub async fn run(flag: Option<&str>) -> Result<()> {
    let dry_run = match flag {
        None => false,
        Some("--dry-run") => true,
        Some(other) => {
            bail!("unknown argument {other}; usage: signet-shop provision-stripe [--dry-run]")
        }
    };
    let api_key =
        std::env::var("STRIPE_API_KEY").context("set STRIPE_API_KEY to provision Stripe")?;
    let stripe = Stripe::connect(&api_key, crate::STRIPE_SERVICE)
        .await
        .context("connect to Stripe")?;

    let path = catalog::default_path();
    let mut text = std::fs::read_to_string(&path)
        .with_context(|| format!("read catalog {}", path.display()))?;
    let cat = catalog::parse(&text).with_context(|| format!("parse catalog {}", path.display()))?;

    let verb = if dry_run { "Previewing" } else { "Reconciling" };
    println!("{verb} {} SKU(s) against Stripe.\n", cat.skus.len());
    println!("{:<32} {:<8} price id", "sku", "status");

    let tax_inclusive = cat.tax.mode == TaxMode::Stripe;
    if dry_run {
        for sku in &cat.skus {
            let (product, price) = stripe
                .preview_price(&price_spec(sku, tax_inclusive))
                .await
                .with_context(|| format!("preview {}", sku.id))?;
            println!("{}", preview_line(sku, &product, &price));
        }
        println!(
            "\nDry run: nothing written to Stripe or {}.",
            path.display()
        );
        return Ok(());
    }
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

/// One dry-run row, labelled like the write run's statuses.
fn preview_line(sku: &Sku, product: &ProductAction, price: &PriceAction) -> String {
    let product = match product {
        ProductAction::Existing(id) => id.clone(),
        ProductAction::Reactivate(id) => format!("{id}, reactivated"),
        ProductAction::Create(id) => format!("{id}, created"),
    };
    match price {
        PriceAction::Unchanged => format!("{:<32} {:<8} {}", sku.id, "ok", sku.stripe_price_id),
        PriceAction::Create => format!("{:<32} {:<8} new price on {product}", sku.id, "set"),
        PriceAction::Supersede { old } => format!(
            "{:<32} {:<8} new price on {product} (replaces {old})",
            sku.id, "updated"
        ),
    }
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
    use stripe_edge::{PriceAction, ProductAction};

    #[test]
    fn preview_line_names_the_action() {
        let cat = crate::catalog::parse(include_str!("../../../catalog.example.toml")).unwrap();
        let mut sku = cat.skus[0].clone();
        sku.stripe_price_id = "price_old".into();
        let id = format!("signet_{}", sku.id);
        let line = |product, price| {
            let l = super::preview_line(&sku, &product, &price);
            l.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")
        };
        assert_eq!(
            line(ProductAction::Existing(id.clone()), PriceAction::Unchanged),
            "ok price_old"
        );
        assert_eq!(
            line(ProductAction::Create(id.clone()), PriceAction::Create),
            format!("set new price on {id}, created")
        );
        assert_eq!(
            line(
                ProductAction::Reactivate(id.clone()),
                PriceAction::Supersede {
                    old: "price_old".into()
                }
            ),
            format!("updated new price on {id}, reactivated (replaces price_old)")
        );
    }

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
