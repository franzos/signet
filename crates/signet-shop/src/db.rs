use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{SqliteConnection, SqlitePool};

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
    /// Net and tax are `None` under `[tax] mode = "none"`; all are `None` on pre-tax rows.
    pub amount_total: Option<i64>,
    pub amount_subtotal: Option<i64>,
    pub amount_tax: Option<i64>,
    pub currency: Option<String>,
    pub country: Option<String>,
    pub vat_id: Option<String>,
    pub tax_exemption: Option<String>,
    pub revoked_at: Option<i64>,
    pub revoked_reason: Option<String>,
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

const LICENSE_COLUMNS: &str =
    "stripe_session_id, license_id, product, sku, customer, email, blob, issued_at,
     amount_total, amount_subtotal, amount_tax, currency, country, vat_id, tax_exemption,
     revoked_at, revoked_reason";

/// Unsent mail at or past this many attempts is dead: no longer selected for delivery.
pub const MAX_MAIL_ATTEMPTS: i64 = 20;

/// An outbox row to write alongside a licence or hold.
#[derive(Debug, Clone)]
pub struct NewMail {
    pub kind: String,
    pub recipient: String,
    pub payload: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Hold {
    pub session_id: String,
    pub reason: String,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboxMail {
    pub id: i64,
    pub kind: String,
    pub recipient: String,
    pub payload: String,
    pub attempts: i64,
}

/// Insert a license, or return the existing row for this session. The boolean is
/// `true` only when this call inserted, so side effects run once even though both
/// the webhook and `/success` reach here for the same session. On insert, the
/// `mail` rows are queued and any open hold for the session is resolved in the
/// same transaction; under a hold recording a refund or dispute the licence is
/// stored revoked and no mail is queued.
pub async fn insert_or_get(
    pool: &SqlitePool,
    rec: &StoredLicense,
    mail: &[NewMail],
) -> Result<(StoredLicense, bool)> {
    let mut tx = pool.begin().await?;
    let inserted = insert_licence(&mut tx, rec, mail).await?;
    let stored = sqlx::query_as::<_, StoredLicense>(&format!(
        "SELECT {LICENSE_COLUMNS} FROM licenses WHERE stripe_session_id = ?"
    ))
    .bind(&rec.stripe_session_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((stored, inserted))
}

/// Excludes holds whose payment was refunded or disputed.
const NOT_REVERSED: &str = "reason NOT LIKE 'refund:%' AND reason NOT LIKE 'dispute:%'";

/// Whether a hold reason records a refund or dispute.
pub fn is_reversed(reason: &str) -> bool {
    reason.starts_with("refund:") || reason.starts_with("dispute:")
}

/// Resolve the session's open hold and insert its licence and `mail` in one
/// transaction. Returns `false`, changing nothing, when there is no open hold or
/// the hold records a refund or dispute.
pub async fn release_hold(
    pool: &SqlitePool,
    rec: &StoredLicense,
    mail: &[NewMail],
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    // Writing first takes the write lock, so a concurrent reversal lands wholly before or after.
    let claimed = sqlx::query(&format!(
        "UPDATE holds SET resolved_at = strftime('%s','now')
         WHERE session_id = ? AND resolved_at IS NULL AND {NOT_REVERSED}"
    ))
    .bind(&rec.stripe_session_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !claimed || !insert_licence(&mut tx, rec, mail).await? {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// Record a refund or dispute on the session's open hold, so it cannot be
/// released. The first reversal recorded stays. Returns whether a hold changed.
pub async fn mark_hold_reversed(pool: &SqlitePool, session_id: &str, reason: &str) -> Result<bool> {
    let done = sqlx::query(&format!(
        "UPDATE holds SET reason = ?
         WHERE session_id = ? AND resolved_at IS NULL AND {NOT_REVERSED}"
    ))
    .bind(reason)
    .bind(session_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    Ok(done)
}

/// Insert a licence; on insert, queue `mail` and resolve any open hold for the
/// session. An open hold recording a refund or dispute stores the licence
/// revoked with that reason and queues no mail.
async fn insert_licence(
    conn: &mut SqliteConnection,
    rec: &StoredLicense,
    mail: &[NewMail],
) -> Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO licenses
           (stripe_session_id, license_id, product, sku, customer, email, blob, issued_at,
            amount_total, amount_subtotal, amount_tax, currency, country, vat_id, tax_exemption)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
    .bind(rec.amount_total)
    .bind(rec.amount_subtotal)
    .bind(rec.amount_tax)
    .bind(&rec.currency)
    .bind(&rec.country)
    .bind(&rec.vat_id)
    .bind(&rec.tax_exemption)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;

    if inserted {
        // Read after the insert: the write lock is held, so a reversal hold cannot land in between.
        let reversal: Option<String> = sqlx::query_scalar(&format!(
            "SELECT reason FROM holds
             WHERE session_id = ? AND resolved_at IS NULL AND NOT ({NOT_REVERSED})"
        ))
        .bind(&rec.stripe_session_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(reason) = reversal {
            revoke(conn, &rec.stripe_session_id, &reason).await?;
        } else {
            for m in mail {
                enqueue(conn, m).await?;
            }
        }
        sqlx::query(
            "UPDATE holds SET resolved_at = strftime('%s','now')
             WHERE session_id = ? AND resolved_at IS NULL",
        )
        .bind(&rec.stripe_session_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(inserted)
}

pub async fn get_by_session(pool: &SqlitePool, session_id: &str) -> Result<Option<StoredLicense>> {
    let row = sqlx::query_as::<_, StoredLicense>(&format!(
        "SELECT {LICENSE_COLUMNS} FROM licenses WHERE stripe_session_id = ?"
    ))
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn enqueue(conn: &mut SqliteConnection, m: &NewMail) -> Result<()> {
    sqlx::query("INSERT INTO mail_outbox (kind, recipient, payload) VALUES (?, ?, ?)")
        .bind(&m.kind)
        .bind(&m.recipient)
        .bind(&m.payload)
        .execute(conn)
        .await?;
    Ok(())
}

/// Record a hold for a session, or reopen a closed one for a different reason,
/// queueing `notify` only then. When the session already has a licence no hold
/// is written; a refund or dispute reason revokes that licence instead. Returns
/// whether a hold was written or a licence revoked.
pub async fn insert_hold(
    pool: &SqlitePool,
    session_id: &str,
    reason: &str,
    notify: Option<NewMail>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let held = sqlx::query(
        "INSERT INTO holds (session_id, reason)
         SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM licenses WHERE stripe_session_id = ?1)
         ON CONFLICT(session_id) DO UPDATE
           SET reason = excluded.reason, created_at = strftime('%s','now'), resolved_at = NULL
           WHERE holds.resolved_at IS NOT NULL AND holds.reason <> excluded.reason",
    )
    .bind(session_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    let recorded = if held {
        if let Some(m) = notify {
            enqueue(&mut tx, &m).await?;
        }
        true
    } else if is_reversed(reason) {
        revoke(&mut tx, session_id, reason).await?
    } else {
        false
    };
    tx.commit().await?;
    Ok(recorded)
}

pub async fn open_hold(pool: &SqlitePool, session_id: &str) -> Result<Option<Hold>> {
    let row = sqlx::query_as::<_, Hold>(
        "SELECT session_id, reason, created_at, resolved_at
         FROM holds WHERE session_id = ? AND resolved_at IS NULL",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Resolve an open hold without a licence. Returns whether one was open.
pub async fn resolve_hold(pool: &SqlitePool, session_id: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE holds SET resolved_at = strftime('%s','now')
         WHERE session_id = ? AND resolved_at IS NULL",
    )
    .bind(session_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    Ok(done)
}

/// Open holds, or every hold when `all` is set, oldest first.
pub async fn list_holds(pool: &SqlitePool, all: bool) -> Result<Vec<Hold>> {
    let rows = sqlx::query_as::<_, Hold>(
        "SELECT session_id, reason, created_at, resolved_at
         FROM holds WHERE ? OR resolved_at IS NULL ORDER BY created_at, id",
    )
    .bind(all)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Mark a session's licence revoked. Returns `false` when there is no licence or
/// it is already revoked, so the first reason recorded stays.
async fn revoke(conn: &mut SqliteConnection, session_id: &str, reason: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE licenses SET revoked_at = strftime('%s','now'), revoked_reason = ?
         WHERE stripe_session_id = ? AND revoked_at IS NULL",
    )
    .bind(reason)
    .bind(session_id)
    .execute(conn)
    .await?
    .rows_affected()
        == 1;
    Ok(done)
}

pub async fn list_revoked(pool: &SqlitePool) -> Result<Vec<StoredLicense>> {
    let rows = sqlx::query_as::<_, StoredLicense>(&format!(
        "SELECT {LICENSE_COLUMNS} FROM licenses WHERE revoked_at IS NOT NULL ORDER BY revoked_at, id"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Unsent, live outbox rows whose next attempt is at or before `now`.
pub async fn due_mail(pool: &SqlitePool, now: i64, max: i64) -> Result<Vec<OutboxMail>> {
    let rows = sqlx::query_as::<_, OutboxMail>(
        "SELECT id, kind, recipient, payload, attempts FROM mail_outbox
         WHERE sent_at IS NULL AND next_attempt_at <= ? AND attempts < ?
         ORDER BY next_attempt_at, id LIMIT ?",
    )
    .bind(now)
    .bind(MAX_MAIL_ATTEMPTS)
    .bind(max)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn mark_sent(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE mail_outbox SET sent_at = strftime('%s','now') WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_failed(pool: &SqlitePool, id: i64, next_at: i64) -> Result<()> {
    sqlx::query("UPDATE mail_outbox SET attempts = attempts + 1, next_attempt_at = ? WHERE id = ?")
        .bind(next_at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Unsent rows that ran out of attempts.
pub async fn dead_mail_count(pool: &SqlitePool) -> Result<i64> {
    let n = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mail_outbox WHERE sent_at IS NULL AND attempts >= ?",
    )
    .bind(MAX_MAIL_ATTEMPTS)
    .fetch_one(pool)
    .await?;
    Ok(n)
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
            amount_total: Some(49900),
            amount_subtotal: Some(40569),
            amount_tax: Some(9331),
            currency: Some("eur".into()),
            country: Some("DE".into()),
            vat_id: None,
            tax_exemption: None,
            revoked_at: None,
            revoked_reason: None,
        }
    }

    #[tokio::test]
    async fn insert_or_get_is_idempotent_on_session_id() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let (first, inserted) = insert_or_get(&pool, &rec("cs_1", "lic_a"), &[])
            .await
            .unwrap();
        assert_eq!(first.license_id, "lic_a");
        assert!(inserted, "first insert is fresh");
        // Second trigger for the SAME session with a DIFFERENT freshly-minted
        // license must return the already-stored one, never the newcomer, and
        // report that nothing new was inserted.
        let (second, inserted) = insert_or_get(&pool, &rec("cs_1", "lic_b"), &[])
            .await
            .unwrap();
        assert_eq!(second.license_id, "lic_a");
        assert_eq!(second.blob, "blob-for-lic_a");
        assert!(!inserted, "second call must not insert");
    }

    #[tokio::test]
    async fn hold_is_recorded_once_and_resolved_by_the_licence() {
        let pool = connect("sqlite::memory:").await.unwrap();
        assert!(insert_hold(&pool, "cs_h", "missing_sku", None)
            .await
            .unwrap());
        assert!(!insert_hold(&pool, "cs_h", "unknown_sku", None)
            .await
            .unwrap());
        let hold = open_hold(&pool, "cs_h").await.unwrap().expect("open hold");
        assert_eq!(hold.reason, "missing_sku");

        insert_or_get(&pool, &rec("cs_h", "lic_h"), &[])
            .await
            .unwrap();
        assert!(open_hold(&pool, "cs_h").await.unwrap().is_none());
        assert!(list_holds(&pool, false).await.unwrap().is_empty());
        let all = list_holds(&pool, true).await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].resolved_at.is_some());
    }

    #[tokio::test]
    async fn licence_under_a_reversed_hold_is_stored_revoked_without_mail() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_hold(&pool, "cs_rev", "refund:ch_1", None)
            .await
            .unwrap();
        let mail = ["license", "sale_notice"].map(|kind| NewMail {
            kind: kind.into(),
            recipient: "a@b.c".into(),
            payload: "{}".into(),
        });
        let (stored, inserted) = insert_or_get(&pool, &rec("cs_rev", "lic_rev"), &mail)
            .await
            .unwrap();
        assert!(inserted);
        assert_eq!(stored.revoked_reason.as_deref(), Some("refund:ch_1"));
        assert!(stored.revoked_at.is_some());
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued, 0);
        assert!(open_hold(&pool, "cs_rev").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reversal_hold_on_a_licence_revokes_and_keeps_the_first_reason() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_or_get(&pool, &rec("cs_r", "lic_r"), &[])
            .await
            .unwrap();
        assert!(insert_hold(&pool, "cs_r", "refund:ch_1", None)
            .await
            .unwrap());
        assert!(!insert_hold(&pool, "cs_r", "dispute:dp_1", None)
            .await
            .unwrap());
        let revoked = list_revoked(&pool).await.unwrap();
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].revoked_reason.as_deref(), Some("refund:ch_1"));
        assert!(revoked[0].revoked_at.is_some());
        assert!(list_holds(&pool, true).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ordinary_hold_on_a_licence_is_skipped() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_or_get(&pool, &rec("cs_l", "lic_l"), &[])
            .await
            .unwrap();
        let notice = NewMail {
            kind: "hold_notice".into(),
            recipient: "ops@b.c".into(),
            payload: "{}".into(),
        };
        assert!(!insert_hold(&pool, "cs_l", "lookup_failed", Some(notice))
            .await
            .unwrap());
        assert!(list_holds(&pool, true).await.unwrap().is_empty());
        assert!(list_revoked(&pool).await.unwrap().is_empty());
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued, 0);
    }

    #[tokio::test]
    async fn closed_hold_reopens_on_a_new_failure() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_hold(&pool, "cs_o", "missing_sku", None)
            .await
            .unwrap();
        assert!(resolve_hold(&pool, "cs_o").await.unwrap());
        sqlx::query("UPDATE holds SET created_at = 1000")
            .execute(&pool)
            .await
            .unwrap();
        let notice = NewMail {
            kind: "hold_notice".into(),
            recipient: "ops@b.c".into(),
            payload: "{}".into(),
        };
        assert!(insert_hold(&pool, "cs_o", "refund:ch_o", Some(notice))
            .await
            .unwrap());
        let hold = open_hold(&pool, "cs_o").await.unwrap().expect("reopened");
        assert_eq!(hold.reason, "refund:ch_o");
        assert!(hold.created_at > 1000);
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued, 1);
        assert_eq!(list_holds(&pool, true).await.unwrap().len(), 1);
        assert!(resolve_hold(&pool, "cs_o").await.unwrap());
        assert!(!resolve_hold(&pool, "cs_o").await.unwrap());
    }
}
