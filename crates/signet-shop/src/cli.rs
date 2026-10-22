use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use sqlx::SqlitePool;
use stripe_edge::SessionInfo;

use crate::config::AppConfig;
use crate::db::{self, StoredLicense};
use crate::fulfill::{self, FulfillOutcome, Payment};
use crate::outbox;
use crate::state::AppState;

const RELEASE_USAGE: &str = "usage: signet-shop release <session_id>";
const RECONCILE_USAGE: &str = "usage: signet-shop reconcile --since YYYY-MM-DD";

/// `signet-shop holds [--all]`. Reads the database only; no Stripe connection.
pub async fn holds(flag: Option<&str>) -> Result<()> {
    let all = match flag {
        None => false,
        Some("--all") => true,
        Some(other) => bail!("unknown argument {other}; usage: signet-shop holds [--all]"),
    };
    let cfg = AppConfig::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    print!(
        "{}",
        holds_report(&pool, all, Utc::now().timestamp()).await?
    );
    Ok(())
}

async fn holds_report(pool: &SqlitePool, all: bool, now: i64) -> Result<String> {
    let mut out = String::new();
    let holds = db::list_holds(pool, all).await?;
    let title = if all { "Holds" } else { "Open holds" };
    writeln!(out, "{title}: {}", holds.len())?;
    for h in &holds {
        let state = if h.resolved_at.is_some() {
            "resolved"
        } else {
            "open"
        };
        writeln!(
            out,
            "  {:>5}  {state:<8}  {}  {}",
            age(now - h.created_at),
            h.session_id,
            h.reason
        )?;
    }
    writeln!(out, "Dead mail: {}", db::dead_mail_count(pool).await?)?;
    let revoked = db::list_revoked(pool).await?;
    writeln!(out, "Revoked licences: {}", revoked.len())?;
    for lic in &revoked {
        writeln!(
            out,
            "  {}  {}  {}",
            lic.revoked_at.map_or_else(|| "-".into(), utc),
            lic.stripe_session_id,
            lic.revoked_reason.as_deref().unwrap_or("-")
        )?;
    }
    Ok(out)
}

fn age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

fn utc(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).map_or_else(
        || ts.to_string(),
        |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// `signet-shop release <session_id>`.
pub async fn release(session_id: Option<&str>) -> Result<()> {
    let id = session_id.ok_or_else(|| anyhow!(RELEASE_USAGE))?;
    if id.starts_with("charge:") {
        let pool = db::connect(&AppConfig::from_env()?.database_url).await?;
        println!("{}", close_charge_hold(&pool, id).await?);
        return Ok(());
    }
    let state = AppState::build(AppConfig::from_env()?).await?;
    releasable(&state, id).await?;
    let session = (state.lookups.session)(&state, id)
        .await
        .context("retrieve the session from Stripe")?
        .ok_or_else(|| anyhow!("Stripe has no session {id}"))?;
    let rec = release_paid(&state, &session).await?;
    println!(
        "Released {id}: licence {} ({}) for {}, {} mail(s) queued",
        rec.license_id,
        rec.sku,
        rec.customer,
        outbox::rows_for(&state, &rec).len()
    );
    Ok(())
}

/// Resolves an open `charge:<id>` hold; it has no session to mint for.
async fn close_charge_hold(pool: &SqlitePool, id: &str) -> Result<String> {
    if !db::resolve_hold(pool, id).await? {
        bail!("no open hold for {id}");
    }
    Ok(format!(
        "hold {id} closed: it recorded a refund or dispute that could not be matched to a sale; \
         check the charge in Stripe and revoke any licence by hand"
    ))
}

/// Errors unless the session has an open hold that records no refund or dispute.
async fn releasable(state: &AppState, session_id: &str) -> Result<()> {
    let hold = db::open_hold(&state.db, session_id)
        .await?
        .ok_or_else(|| anyhow!("no open hold for {session_id}"))?;
    if db::is_reversed(&hold.reason) {
        bail!(
            "hold for {session_id} records {}: the payment was refunded or disputed, not releasing",
            hold.reason
        );
    }
    Ok(())
}

/// Mints the licence for a held, paid session without the economics check,
/// resolving the hold and queueing its mail in one transaction.
pub(crate) async fn release_paid(state: &AppState, session: &SessionInfo) -> Result<StoredLicense> {
    releasable(state, &session.id).await?;
    if !session.paid {
        bail!("session {} is not paid", session.id);
    }
    match fulfill::check_payment(state, session).await? {
        Payment::Clear => {}
        Payment::Reversed(reason) => {
            db::mark_hold_reversed(&state.db, &session.id, &reason).await?;
            bail!(
                "the payment for {} records {reason}: refunded or disputed, not releasing",
                session.id
            );
        }
        Payment::Unknown => bail!(
            "could not read the charge for {} from Stripe; see the log",
            session.id
        ),
    }
    let rec = fulfill::prepare(state, session, true)
        .map_err(|reason| anyhow!("cannot issue a licence for {}: {reason}", session.id))?;
    let mail = outbox::rows_for(state, &rec);
    if !db::release_hold(&state.db, &rec, &mail).await? {
        bail!(
            "the hold for {} changed; run `signet-shop holds`",
            session.id
        );
    }
    Ok(rec)
}

/// `signet-shop reconcile --since YYYY-MM-DD`.
pub async fn reconcile(flag: Option<&str>, date: Option<&str>) -> Result<()> {
    let (Some("--since"), Some(date)) = (flag, date) else {
        bail!(RECONCILE_USAGE);
    };
    let since = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .with_context(|| format!("invalid date {date}; {RECONCILE_USAGE}"))?
        .and_time(NaiveTime::MIN)
        .and_utc()
        .timestamp();
    let state = AppState::build(AppConfig::from_env()?).await?;
    let sessions = state
        .stripe
        .paid_sessions_since(since)
        .await
        .context("list paid Checkout Sessions")?;
    let t = reconcile_sessions(&state, &sessions).await;
    println!(
        "Paid sessions since {date}: {}\n  fulfilled:       {}\n  already present: {}\n  \
         held:            {}\n  errors:          {}",
        sessions.len(),
        t.fulfilled,
        t.present,
        t.held,
        t.errors
    );
    if t.errors > 0 {
        bail!("{} session(s) failed; see the log", t.errors);
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    fulfilled: usize,
    present: usize,
    held: usize,
    errors: usize,
}

/// Runs each paid session through the webhook's fulfil path.
async fn reconcile_sessions(state: &AppState, sessions: &[SessionInfo]) -> Tally {
    let mut t = Tally::default();
    for s in sessions {
        let present = match db::get_by_session(&state.db, &s.id).await {
            Ok(found) => found.is_some(),
            Err(e) => {
                tracing::error!("reconcile: licence lookup failed: {e:#}");
                t.errors += 1;
                continue;
            }
        };
        match fulfill::fulfill_paid_session(state, s).await {
            Ok(FulfillOutcome::Ready(_)) if present => t.present += 1,
            Ok(FulfillOutcome::Ready(_)) => t.fulfilled += 1,
            Ok(FulfillOutcome::Held) => t.held += 1,
            Ok(FulfillOutcome::Pending | FulfillOutcome::LookupFailed) => {
                tracing::error!("reconcile: paid session not fulfilled");
                t.errors += 1;
            }
            Err(e) => {
                tracing::error!("reconcile: fulfil failed: {e:#}");
                t.errors += 1;
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fulfill::tests::{paid_session, stubbed_state, test_state};

    const SKU: &str = "acme-business-annual";

    #[tokio::test]
    async fn reconcile_counts_each_outcome() {
        let state = test_state().await;
        let done = paid_session("cs_done", SKU);
        fulfill::fulfill_paid_session(&state, &done).await.unwrap();
        let mut under = paid_session("cs_under", SKU);
        under.amount_total = Some(1);
        let mut ours = paid_session("cs_ours", SKU);
        ours.metadata.insert("service".into(), "signet".into());
        let sessions = [paid_session("cs_new", SKU), done, under, ours];

        let first = reconcile_sessions(&state, &sessions).await;
        assert_eq!(
            first,
            Tally {
                fulfilled: 2,
                present: 1,
                held: 1,
                errors: 0
            }
        );

        let again = reconcile_sessions(&state, &sessions).await;
        assert_eq!(
            again,
            Tally {
                present: 3,
                held: 1,
                ..Tally::default()
            }
        );

        state.db.close().await;
        let failed = reconcile_sessions(&state, &sessions[..2]).await;
        assert_eq!(
            failed,
            Tally {
                errors: 2,
                ..Tally::default()
            }
        );
    }

    #[tokio::test]
    async fn release_requires_an_open_hold_and_a_paid_session() {
        let state = test_state().await;
        let s = paid_session("cs_nohold", SKU);
        let err = release_paid(&state, &s).await.unwrap_err();
        assert_eq!(err.to_string(), "no open hold for cs_nohold");

        let mut s = paid_session("cs_unpaid", SKU);
        s.metadata.clear();
        fulfill::fulfill_paid_session(&state, &s).await.unwrap();
        s.paid = false;
        let err = release_paid(&state, &s).await.unwrap_err();
        assert_eq!(err.to_string(), "session cs_unpaid is not paid");

        s.paid = true;
        let err = release_paid(&state, &s).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot issue a licence for cs_unpaid: missing_sku"
        );
        assert!(db::open_hold(&state.db, "cs_unpaid")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn release_refuses_a_refunded_payment() {
        let state = stubbed_state().await;
        let mut s = paid_session("cs_release_refunded", SKU);
        s.amount_total = Some(1);
        fulfill::fulfill_paid_session(&state, &s).await.unwrap();
        s.payment_intent_id = Some("pi_refunded".into());
        let err = release_paid(&state, &s).await.unwrap_err();
        assert!(err.to_string().contains("refund:ch_refunded"), "{err:#}");
        let hold = db::open_hold(&state.db, &s.id).await.unwrap().unwrap();
        assert_eq!(hold.reason, "refund:ch_refunded");
        assert!(db::get_by_session(&state.db, &s.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn release_refuses_an_unreadable_charge_and_keeps_the_hold() {
        let state = stubbed_state().await;
        let mut s = paid_session("cs_release_permanent", SKU);
        s.amount_total = Some(1);
        fulfill::fulfill_paid_session(&state, &s).await.unwrap();
        let before = db::open_hold(&state.db, &s.id).await.unwrap().unwrap();
        s.payment_intent_id = Some("pi_permanent".into());
        let err = release_paid(&state, &s).await.unwrap_err();
        assert!(
            err.to_string().contains("could not read the charge"),
            "{err:#}"
        );
        let after = db::open_hold(&state.db, &s.id).await.unwrap().unwrap();
        assert_eq!(after.reason, before.reason);
        assert!(after.resolved_at.is_none());
        assert!(db::get_by_session(&state.db, &s.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn reconcile_holds_a_refunded_session() {
        let state = stubbed_state().await;
        let mut s = paid_session("cs_reconcile_refunded", SKU);
        s.payment_intent_id = Some("pi_refunded".into());
        assert_eq!(
            reconcile_sessions(&state, &[s.clone()]).await,
            Tally {
                held: 1,
                ..Tally::default()
            }
        );
        let hold = db::open_hold(&state.db, &s.id).await.unwrap().unwrap();
        assert_eq!(hold.reason, "refund:ch_refunded");
    }

    #[tokio::test]
    async fn charge_hold_is_closed_without_a_licence() {
        let state = test_state().await;
        db::insert_hold(&state.db, "charge:ch_x", "lookup_failed:permanent", None)
            .await
            .unwrap();
        let msg = close_charge_hold(&state.db, "charge:ch_x").await.unwrap();
        assert!(msg.starts_with("hold charge:ch_x closed:"), "{msg}");
        assert!(db::open_hold(&state.db, "charge:ch_x")
            .await
            .unwrap()
            .is_none());
        let err = close_charge_hold(&state.db, "charge:ch_x")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no open hold for charge:ch_x");
    }

    #[tokio::test]
    async fn holds_report_lists_holds_dead_mail_and_revocations() {
        let state = test_state().await;
        db::insert_hold(&state.db, "cs_h", "missing_sku", None)
            .await
            .unwrap();
        db::insert_hold(&state.db, "charge:ch_x", "lookup_failed:permanent", None)
            .await
            .unwrap();
        fulfill::fulfill_paid_session(&state, &paid_session("cs_r", SKU))
            .await
            .unwrap();
        db::revoke(&state.db, "cs_r", "refund:ch_1").await.unwrap();
        sqlx::query("UPDATE holds SET created_at = 1000")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("UPDATE licenses SET revoked_at = 1700000000")
            .execute(&state.db)
            .await
            .unwrap();

        let out = holds_report(&state.db, false, 1000 + 3 * 3600)
            .await
            .unwrap();
        assert_eq!(
            out,
            "Open holds: 2\n     3h  open      cs_h  missing_sku\n\
             \x20    3h  open      charge:ch_x  lookup_failed:permanent\nDead mail: 0\n\
             Revoked licences: 1\n  2023-11-14 22:13 UTC  cs_r  refund:ch_1\n"
        );
    }

    #[test]
    fn age_is_human_readable() {
        assert_eq!(age(-5), "0s");
        assert_eq!(age(59), "59s");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3 * 3600 + 59), "3h");
        assert_eq!(age(2 * 86_400), "2d");
    }
}
