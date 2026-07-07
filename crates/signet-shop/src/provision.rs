use anyhow::{Context, Result};

use crate::catalog::{self, Sku};
use crate::payments;

/// What reconciling one SKU against Stripe did to its `stripe_price_id`.
enum Change {
    /// The catalog id already points at a live price with the same amount.
    Unchanged,
    /// The id was empty; we created (or reused) a price and filled it in.
    Set(String),
    /// The amount/currency changed; a new price supersedes `old` and the old
    /// price was archived.
    Updated { old: String, new: String },
}

/// `signet-shop provision-stripe`: reconcile every SKU in `catalog.toml` with
/// Stripe. For each SKU we create (or reuse) a Product + Price and write the
/// resolved `stripe_price_id` back into the file, preserving comments. It is
/// idempotent: an unchanged catalog produces no Stripe calls that mutate state
/// and no file write.
pub async fn run() -> Result<()> {
    let api_key = std::env::var("STRIPE_API_KEY")
        .or_else(|_| std::env::var("STRIPE_SECRET_KEY"))
        .context("set STRIPE_API_KEY (or STRIPE_SECRET_KEY) to provision Stripe")?;
    let client = stripe::ClientBuilder::new(api_key)
        .build()
        .context("build stripe client")?;

    let path = catalog::default_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read catalog {}", path.display()))?;
    let cat = catalog::parse(&text).with_context(|| format!("parse catalog {}", path.display()))?;

    // Editable view of the same bytes, so we only touch the price id fields.
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {} as editable TOML", path.display()))?;

    println!("Reconciling {} SKU(s) against Stripe.\n", cat.skus.len());
    println!("{:<32} {:<8} price id", "sku", "status");

    let mut changed = false;
    for sku in &cat.skus {
        let change = reconcile_sku(&client, sku)
            .await
            .with_context(|| format!("reconcile {}", sku.id))?;
        match change {
            Change::Unchanged => {
                println!("{:<32} {:<8} {}", sku.id, "ok", sku.stripe_price_id);
            }
            Change::Set(id) => {
                set_price_id(&mut doc, &sku.id, &id);
                changed = true;
                println!("{:<32} {:<8} {}", sku.id, "set", id);
            }
            Change::Updated { old, new } => {
                set_price_id(&mut doc, &sku.id, &new);
                changed = true;
                println!("{:<32} {:<8} {} (was {})", sku.id, "updated", new, old);
            }
        }
    }

    if changed {
        std::fs::write(&path, doc.to_string())
            .with_context(|| format!("write catalog {}", path.display()))?;
        println!("\nWrote resolved price ids to {}.", path.display());
    } else {
        println!("\nNo changes: {} already matches Stripe.", path.display());
    }
    Ok(())
}

/// Reconcile a single SKU. Reuses the price already named in the catalog when it
/// still matches, otherwise creates one (archiving a superseded price).
async fn reconcile_sku(client: &stripe::Client, sku: &Sku) -> Result<Change> {
    // Detect what is already there: if the catalog names a live price with the
    // same amount/currency, there is nothing to do (and nothing to write).
    let mut old_exists = false;
    if !sku.stripe_price_id.is_empty() {
        if let Some(existing) = payments::retrieve_price(client, &sku.stripe_price_id).await? {
            old_exists = true;
            if existing.active
                && existing.unit_amount == Some(sku.amount_cents)
                && existing.currency == sku.currency
            {
                return Ok(Change::Unchanged);
            }
        }
    }

    let product = payments::ensure_product(client, &sku.id, &sku.display_name)
        .await
        .context("ensure product")?;
    let price = payments::ensure_price(client, &product, sku.amount_cents, sku.currency.clone())
        .await
        .context("ensure price")?;

    if sku.stripe_price_id.is_empty() {
        Ok(Change::Set(price))
    } else if price == sku.stripe_price_id {
        Ok(Change::Unchanged)
    } else {
        // The amount/currency changed. Archive the old price (if it still
        // exists) so it can no longer be purchased, and record the new one.
        if old_exists {
            payments::deactivate_price(client, &sku.stripe_price_id)
                .await
                .context("archive superseded price")?;
        }
        Ok(Change::Updated {
            old: sku.stripe_price_id.clone(),
            new: price,
        })
    }
}

/// Set `stripe_price_id` on the `[[sku]]` whose `id` matches, preserving the
/// key's existing decor (alignment and any trailing comment).
fn set_price_id(doc: &mut toml_edit::DocumentMut, sku_id: &str, price_id: &str) {
    let Some(array) = doc.get_mut("sku").and_then(|i| i.as_array_of_tables_mut()) else {
        return;
    };
    for table in array.iter_mut() {
        if table.get("id").and_then(|v| v.as_str()) != Some(sku_id) {
            continue;
        }
        match table
            .get_mut("stripe_price_id")
            .and_then(|i| i.as_value_mut())
        {
            Some(v) => {
                let decor = v.decor().clone();
                *v = toml_edit::Value::from(price_id);
                *v.decor_mut() = decor;
            }
            None => {
                table["stripe_price_id"] = toml_edit::value(price_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::set_price_id;

    #[test]
    fn set_price_id_preserves_comments_and_only_touches_match() {
        let src = r#"[[sku]]
id = "a"
stripe_price_id = ""        # fill me in
amount_cents = 100

[[sku]]
id = "b"
stripe_price_id = "price_keep"
amount_cents = 200
"#;
        let mut doc = src.parse::<toml_edit::DocumentMut>().unwrap();
        set_price_id(&mut doc, "a", "price_new");
        let out = doc.to_string();

        // The matched sku got the new id, keeping its trailing comment/alignment.
        assert!(
            out.contains(r#"stripe_price_id = "price_new"        # fill me in"#),
            "got:\n{out}"
        );
        // The other sku is untouched.
        assert!(out.contains(r#"stripe_price_id = "price_keep""#));
        // Nothing else moved.
        assert!(out.contains("amount_cents = 100"));
        assert!(out.contains("amount_cents = 200"));
    }

    #[test]
    fn set_price_id_inserts_when_field_absent() {
        let src = "[[sku]]\nid = \"a\"\namount_cents = 100\n";
        let mut doc = src.parse::<toml_edit::DocumentMut>().unwrap();
        set_price_id(&mut doc, "a", "price_x");
        assert!(doc.to_string().contains(r#"stripe_price_id = "price_x""#));
    }
}
