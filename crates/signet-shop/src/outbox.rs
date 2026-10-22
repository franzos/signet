//! Durable purchase and hold mail. Rows are queued in the same transaction as the
//! licence or hold; `drain_loop` rebuilds each email at send time from the stored
//! licence, so a payload never carries the licence blob.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use polymail::{Address, Email};
use serde::{Deserialize, Serialize};

use crate::catalog::Sku;
use crate::db::{self, NewMail, OutboxMail, StoredLicense};
use crate::mail::{MailService, PurchaseNotice, Receipt};
use crate::state::AppState;

pub const LICENSE: &str = "license";
pub const SALE_NOTICE: &str = "sale_notice";
pub const HOLD_NOTICE: &str = "hold_notice";

const DRAIN_INTERVAL: Duration = Duration::from_secs(15);
const DRAIN_BATCH: i64 = 12;
const BACKOFF_BASE_SECS: i64 = 30;
const BACKOFF_CAP_SECS: i64 = 21_600;

/// Payload of `license` and `sale_notice` rows.
#[derive(Serialize, Deserialize)]
struct SalePayload {
    session_id: String,
}

/// Payload of `hold_notice` rows.
#[derive(Serialize, Deserialize)]
struct HoldPayload {
    session_id: String,
    reason: String,
}

/// Outbox rows for a new licence; empty when mail is off.
pub fn rows_for(state: &AppState, rec: &StoredLicense) -> Vec<NewMail> {
    let Some(mail) = &state.mail else {
        return Vec::new();
    };
    let payload = to_json(&SalePayload {
        session_id: rec.stripe_session_id.clone(),
    });
    let mut rows = Vec::new();
    if !rec.email.is_empty() {
        rows.push(NewMail {
            kind: LICENSE.into(),
            recipient: rec.email.clone(),
            payload: payload.clone(),
        });
    }
    if let Some(op) = mail.operator_address() {
        rows.push(NewMail {
            kind: SALE_NOTICE.into(),
            recipient: op.email.clone(),
            payload,
        });
    }
    rows
}

/// The operator notice for a new hold; `None` when mail or the operator address is off.
pub fn hold_row(state: &AppState, session_id: &str, reason: &str) -> Option<NewMail> {
    let op = state.mail.as_ref()?.operator_address()?;
    Some(NewMail {
        kind: HOLD_NOTICE.into(),
        recipient: op.email.clone(),
        payload: to_json(&HoldPayload {
            session_id: session_id.into(),
            reason: reason.into(),
        }),
    })
}

fn to_json<T: Serialize>(v: &T) -> String {
    // Serializing a struct of plain strings cannot fail.
    serde_json::to_string(v).expect("outbox payload serializes")
}

pub async fn drain_loop(state: AppState) {
    let mut tick = tokio::time::interval(DRAIN_INTERVAL);
    loop {
        tick.tick().await;
        if let Err(e) = drain_once(&state, Utc::now().timestamp()).await {
            tracing::error!("mail outbox drain failed: {e:#}");
        }
    }
}

/// Seconds until the next attempt, given the attempts made before this failure.
fn backoff(attempts: i64) -> i64 {
    // 30 << 10 already exceeds the cap; clamping keeps the shift in range.
    (BACKOFF_BASE_SECS << attempts.clamp(0, 10)).min(BACKOFF_CAP_SECS)
}

pub(crate) async fn drain_once(state: &AppState, now: i64) -> Result<()> {
    let Some(mail) = &state.mail else {
        return Ok(());
    };
    for row in db::due_mail(&state.db, now, DRAIN_BATCH).await? {
        let Some(email) = build(state, mail, &row).await? else {
            // Retrying cannot help; marking it sent stops selection without counting it dead.
            tracing::error!(id = row.id, kind = %row.kind, "closing undeliverable outbox row");
            db::mark_sent(&state.db, row.id).await?;
            continue;
        };
        match mail.send(&email).await {
            Ok(()) => db::mark_sent(&state.db, row.id).await?,
            Err(e) => {
                tracing::warn!(
                    id = row.id,
                    kind = %row.kind,
                    attempts = row.attempts + 1,
                    retryable = e.is_retryable(),
                    "outbox send failed"
                );
                db::mark_failed(&state.db, row.id, now + backoff(row.attempts)).await?;
            }
        }
    }
    Ok(())
}

/// The email for a row, or `None` when the row can never be sent.
async fn build(state: &AppState, mail: &MailService, row: &OutboxMail) -> Result<Option<Email>> {
    // The configured address carries the operator's display name.
    let to = match mail.operator_address() {
        Some(op) if op.email == row.recipient => op.clone(),
        _ => Address::new(row.recipient.clone()),
    };
    if row.kind == HOLD_NOTICE {
        let Ok(p) = serde_json::from_str::<HoldPayload>(&row.payload) else {
            return Ok(None);
        };
        return Ok(mail.build_hold_email(&to, &p.session_id, &p.reason).ok());
    }
    if row.kind != LICENSE && row.kind != SALE_NOTICE {
        return Ok(None);
    }
    let Ok(p) = serde_json::from_str::<SalePayload>(&row.payload) else {
        return Ok(None);
    };
    let Some(lic) = db::get_by_session(&state.db, &p.session_id).await? else {
        return Ok(None);
    };
    let Some(sku) = state.catalog.by_id(&lic.sku) else {
        return Ok(None);
    };
    let notice = notice(state, sku, &lic);
    let email = if row.kind == LICENSE {
        mail.build_license_email(&notice)
    } else {
        mail.build_operator_email(&to, &notice)
    };
    Ok(email.ok())
}

fn notice(state: &AppState, sku: &Sku, lic: &StoredLicense) -> PurchaseNotice {
    // Expiry derives from the SKU term and the issue time, as at issue.
    let expires_at = sku
        .to_issue_params(String::new(), String::new(), lic.issued_at, String::new())
        .expires_at;
    PurchaseNotice {
        buyer_email: lic.email.clone(),
        customer: lic.customer.clone(),
        product_display: sku.display_name.clone(),
        license_id: lic.license_id.clone(),
        license_blob: lic.blob.clone(),
        expires_at,
        price_label: price_label(sku),
        receipt: Receipt::build(&state.catalog.tax, lic),
    }
}

/// The catalog label when set, otherwise derived from the SKU's amount and currency.
fn price_label(sku: &Sku) -> String {
    if sku.price_label.is_empty() {
        crate::catalog::format_amount(&sku.currency, sku.amount_cents)
    } else {
        sku.price_label.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use polymail::{Mailer, SendError, SendResult};

    use super::*;
    use crate::fulfill::tests::{paid_session, test_state};
    use crate::fulfill::{fulfill_paid_session, FulfillOutcome};

    /// Records the subject of each delivered email; fails every send while `fail` is set.
    #[derive(Clone, Default)]
    pub(crate) struct TestMailer {
        pub fail: Arc<AtomicBool>,
        pub sent: Arc<Mutex<Vec<String>>>,
    }

    impl Mailer for TestMailer {
        fn send<'a, 'b, 'c>(
            &'a self,
            email: &'b Email,
        ) -> Pin<Box<dyn Future<Output = Result<SendResult, SendError>> + Send + 'c>>
        where
            'a: 'c,
            'b: 'c,
            Self: 'c,
        {
            let fail = self.fail.load(Ordering::SeqCst);
            if !fail {
                self.sent.lock().unwrap().push(email.subject.clone());
            }
            Box::pin(async move {
                if fail {
                    Err(SendError::ServiceUnavailable("down".into()))
                } else {
                    Ok(SendResult { message_id: None })
                }
            })
        }
    }

    pub(crate) fn with_mail(mut state: AppState, mailer: &TestMailer) -> AppState {
        state.mail = Some(Arc::new(MailService::test_service(Box::new(
            mailer.clone(),
        ))));
        state
    }

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct Row {
        kind: String,
        attempts: i64,
        next_attempt_at: i64,
        sent: bool,
    }

    async fn rows(state: &AppState) -> Vec<Row> {
        sqlx::query_as(
            "SELECT kind, attempts, next_attempt_at, sent_at IS NOT NULL AS sent
             FROM mail_outbox ORDER BY id",
        )
        .fetch_all(&state.db)
        .await
        .unwrap()
    }

    fn row(kind: &str, attempts: i64, next_attempt_at: i64, sent: bool) -> Row {
        Row {
            kind: kind.into(),
            attempts,
            next_attempt_at,
            sent,
        }
    }

    /// Queues a licence's mail and pins every row's first attempt to `at`.
    async fn sold(state: &AppState, session_id: &str, at: i64) {
        let s = paid_session(session_id, "acme-business-annual");
        assert!(matches!(
            fulfill_paid_session(state, &s).await.unwrap(),
            FulfillOutcome::Ready(_)
        ));
        sqlx::query("UPDATE mail_outbox SET next_attempt_at = ?")
            .bind(at)
            .execute(&state.db)
            .await
            .unwrap();
    }

    #[test]
    fn backoff_doubles_from_thirty_seconds_up_to_six_hours() {
        assert_eq!(backoff(0), 30);
        assert_eq!(backoff(1), 60);
        assert_eq!(backoff(9), 15_360);
        assert_eq!(backoff(10), 21_600);
        assert_eq!(backoff(19), 21_600);
    }

    #[tokio::test]
    async fn mail_off_queues_nothing() {
        let state = test_state().await;
        let s = paid_session("cs_off", "acme-business-annual");
        let rec = crate::fulfill::prepare(&state, &s, false).unwrap();
        assert!(rows_for(&state, &rec).is_empty());
        assert!(hold_row(&state, "cs_off", "missing_sku").is_none());
    }

    #[tokio::test]
    async fn failed_sends_back_off_and_a_success_marks_sent() {
        let mailer = TestMailer::default();
        let state = with_mail(test_state().await, &mailer);
        let t0 = 1_000_000;
        sold(&state, "cs_retry", t0).await;
        assert_eq!(
            rows(&state).await,
            [row(LICENSE, 0, t0, false), row(SALE_NOTICE, 0, t0, false)]
        );

        mailer.fail.store(true, Ordering::SeqCst);
        drain_once(&state, t0).await.unwrap();
        let t1 = t0 + 30;
        assert_eq!(
            rows(&state).await,
            [row(LICENSE, 1, t1, false), row(SALE_NOTICE, 1, t1, false)]
        );
        // Not due yet.
        drain_once(&state, t1 - 1).await.unwrap();
        assert_eq!(rows(&state).await[0], row(LICENSE, 1, t1, false));

        drain_once(&state, t1).await.unwrap();
        let t2 = t1 + 60;
        assert_eq!(rows(&state).await[0], row(LICENSE, 2, t2, false));

        mailer.fail.store(false, Ordering::SeqCst);
        drain_once(&state, t2).await.unwrap();
        assert_eq!(
            rows(&state).await,
            [row(LICENSE, 2, t2, true), row(SALE_NOTICE, 2, t2, true)]
        );
        assert_eq!(
            *mailer.sent.lock().unwrap(),
            [
                "Your Acme Business (annual) license",
                "New license sold: Acme Business (annual)"
            ]
        );
        drain_once(&state, t2 + 100_000).await.unwrap();
        assert_eq!(mailer.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn capped_rows_are_dead_and_not_selected() {
        let mailer = TestMailer::default();
        let state = with_mail(test_state().await, &mailer);
        sold(&state, "cs_dead", 0).await;
        sqlx::query("UPDATE mail_outbox SET attempts = ? WHERE kind = ?")
            .bind(db::MAX_MAIL_ATTEMPTS)
            .bind(LICENSE)
            .execute(&state.db)
            .await
            .unwrap();
        let due = db::due_mail(&state.db, 0, DRAIN_BATCH).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].kind, SALE_NOTICE);
        assert_eq!(db::dead_mail_count(&state.db).await.unwrap(), 1);

        drain_once(&state, 0).await.unwrap();
        assert_eq!(
            *mailer.sent.lock().unwrap(),
            ["New license sold: Acme Business (annual)"]
        );
        assert_eq!(db::dead_mail_count(&state.db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn poison_rows_are_closed_without_sending() {
        let mailer = TestMailer::default();
        let state = with_mail(test_state().await, &mailer);
        let mut rec = crate::fulfill::prepare(
            &state,
            &paid_session("cs_gone_sku", "acme-business-annual"),
            false,
        )
        .unwrap();
        rec.sku = "retired-sku".into();
        let queued = rows_for(&state, &rec);
        db::insert_or_get(&state.db, &rec, &queued).await.unwrap();
        for (kind, payload) in [
            (LICENSE, r#"{"session_id":"cs_no_licence"}"#),
            (SALE_NOTICE, "not json"),
            (HOLD_NOTICE, "{}"),
            ("unknown", "{}"),
        ] {
            sqlx::query(
                "INSERT INTO mail_outbox (kind, recipient, payload, next_attempt_at)
                 VALUES (?, 'ops@example.com', ?, 0)",
            )
            .bind(kind)
            .bind(payload)
            .execute(&state.db)
            .await
            .unwrap();
        }

        drain_once(&state, Utc::now().timestamp()).await.unwrap();
        let after = rows(&state).await;
        assert_eq!(after.len(), 6);
        assert!(after.iter().all(|r| r.sent && r.attempts == 0), "{after:?}");
        assert!(mailer.sent.lock().unwrap().is_empty());
        assert_eq!(db::dead_mail_count(&state.db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn hold_notice_reaches_the_operator() {
        let mailer = TestMailer::default();
        let state = with_mail(test_state().await, &mailer);
        let mut s = paid_session("cs_hold_mail", "acme-business-annual");
        s.amount_total = Some(1);
        for _ in 0..2 {
            assert!(matches!(
                fulfill_paid_session(&state, &s).await.unwrap(),
                FulfillOutcome::Held
            ));
        }
        let queued = rows(&state).await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].kind, HOLD_NOTICE);

        drain_once(&state, Utc::now().timestamp()).await.unwrap();
        assert!(rows(&state).await[0].sent);
        assert_eq!(*mailer.sent.lock().unwrap(), ["Sale held for review"]);
    }
}
