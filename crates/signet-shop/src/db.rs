use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredLicense {
    pub stripe_session_id: String,
    pub license_id: String,
    pub product: String,
    pub sku: String,
    pub customer: String,
    pub email: String,
    pub blob: String,
    pub issued_at: i64,
}

pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    // In-memory (`sqlite::memory:`) gives each physical connection its own
    // private, empty DB, so migrations on one connection are invisible to the
    // next. Pin the pool to a single connection for in-memory (tests); use WAL
    // + a busy timeout for the file DB so the webhook and the /success write
    // race resolve to "wait" instead of SQLITE_BUSY.
    let in_memory = database_url.contains(":memory:");
    let opts: SqliteConnectOptions = database_url
        .parse::<SqliteConnectOptions>()?
        .create_if_missing(true)
        .journal_mode(if in_memory {
            SqliteJournalMode::Memory
        } else {
            SqliteJournalMode::Wal
        })
        .busy_timeout(std::time::Duration::from_secs(5));
    let max = if in_memory { 1 } else { 5 };
    let pool = SqlitePoolOptions::new()
        .max_connections(max)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Insert a license, or return the existing row for this session. The boolean is
/// `true` only when this call actually inserted (the row was new), so the caller
/// can fire side effects like the purchase email exactly once even though both
/// the webhook and `/success` reach here for the same session.
pub async fn insert_or_get(
    pool: &SqlitePool,
    rec: &StoredLicense,
) -> Result<(StoredLicense, bool)> {
    // Insert only if this session id is new; the UNIQUE constraint makes the
    // race safe. Then read back whichever row won.
    let inserted = sqlx::query(
        "INSERT INTO licenses
           (stripe_session_id, license_id, product, sku, customer, email, blob, issued_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(stripe_session_id) DO NOTHING",
    )
    .bind(&rec.stripe_session_id)
    .bind(&rec.license_id)
    .bind(&rec.product)
    .bind(&rec.sku)
    .bind(&rec.customer)
    .bind(&rec.email)
    .bind(&rec.blob)
    .bind(rec.issued_at)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;

    let stored = get_by_session(pool, &rec.stripe_session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("row vanished after insert"))?;
    Ok((stored, inserted))
}

pub async fn get_by_session(pool: &SqlitePool, session_id: &str) -> Result<Option<StoredLicense>> {
    let row = sqlx::query_as::<_, StoredLicense>(
        "SELECT stripe_session_id, license_id, product, sku, customer, email, blob, issued_at
         FROM licenses WHERE stripe_session_id = ?",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(session: &str, license: &str) -> StoredLicense {
        StoredLicense {
            stripe_session_id: session.into(),
            license_id: license.into(),
            product: "acme".into(),
            sku: "acme-business-annual".into(),
            customer: "Acme".into(),
            email: "a@b.c".into(),
            blob: format!("blob-for-{license}"),
            issued_at: 1_000_000,
        }
    }

    #[tokio::test]
    async fn insert_or_get_is_idempotent_on_session_id() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let (first, inserted) = insert_or_get(&pool, &rec("cs_1", "lic_a")).await.unwrap();
        assert_eq!(first.license_id, "lic_a");
        assert!(inserted, "first insert is fresh");
        // Second trigger for the SAME session with a DIFFERENT freshly-minted
        // license must return the already-stored one, never the newcomer, and
        // report that nothing new was inserted.
        let (second, inserted) = insert_or_get(&pool, &rec("cs_1", "lic_b")).await.unwrap();
        assert_eq!(second.license_id, "lic_a");
        assert_eq!(second.blob, "blob-for-lic_a");
        assert!(!inserted, "second call must not insert");
    }
}
