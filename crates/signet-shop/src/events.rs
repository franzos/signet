use anyhow::Result;
use stripe_edge::{ChargeInfo, Event, SessionInfo};

use crate::db;
use crate::fulfill::{self, FulfillOutcome};
use crate::outbox;
use crate::state::AppState;

/// Applies one verified webhook event. An `Err` is retryable; everything that
/// retrying cannot fix is logged and returns `Ok`.
pub async fn handle(state: &AppState, event: Event) -> Result<()> {
    match event {
        Event::SessionCompleted {
            event_id,
            session_id,
            ..
        } => {
            let session = match (state.lookups.session)(state, &session_id).await {
                Ok(Some(s)) => s,
                Ok(None) => {
                    tracing::warn!(%event_id, "completed session not found in Stripe");
                    hold(state, &session_id, "lookup_failed:session").await?;
                    return Ok(());
                }
                Err(e) if !e.retryable() => {
                    tracing::error!(%event_id, "session lookup failed: {e}");
                    hold(state, &session_id, "lookup_failed:session").await?;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            let outcome = match fulfill::fulfill_paid_session(state, &session).await? {
                FulfillOutcome::Ready { .. } => "ready",
                FulfillOutcome::Pending => "pending",
                FulfillOutcome::Held => "held",
            };
            tracing::info!(%event_id, outcome, "checkout session completed");
        }
        Event::SessionFailed { event_id, .. } => {
            tracing::info!(%event_id, "checkout async payment failed");
        }
        Event::SessionExpired { event_id, .. } => {
            tracing::info!(%event_id, "checkout session expired");
        }
        Event::Refunded {
            event_id,
            charge_id,
            ..
        } => {
            let Some((session, charge)) = resolve(state, &event_id, &charge_id).await? else {
                return Ok(());
            };
            if charge.amount_refunded < charge.amount {
                tracing::info!(%event_id, %charge_id, "partial refund, licence kept");
                return Ok(());
            }
            revoke(state, &event_id, &session, &format!("refund:{charge_id}")).await?;
        }
        Event::Disputed {
            event_id,
            dispute_id,
            charge_id,
            ..
        } => {
            let Some((session, _)) = resolve(state, &event_id, &charge_id).await? else {
                return Ok(());
            };
            revoke(state, &event_id, &session, &format!("dispute:{dispute_id}")).await?;
        }
        Event::Subscription { event_id, .. } => {
            tracing::info!(%event_id, "subscription event not handled");
        }
        Event::Ignored { event_id, kind } => {
            tracing::debug!(%event_id, %kind, "webhook event ignored");
        }
    }
    Ok(())
}

/// This shop's session behind a charge. `None` for another service's session,
/// a charge with no session (not a Checkout sale), and a permanent lookup
/// failure, which holds `charge:<id>`. Transient Stripe failures propagate.
async fn resolve(
    state: &AppState,
    event_id: &str,
    charge_id: &str,
) -> Result<Option<(SessionInfo, ChargeInfo)>> {
    match (state.lookups.charge)(state, charge_id).await {
        Ok(Some((session, _))) if !session.belongs_to(crate::STRIPE_SERVICE) => {
            tracing::debug!(%event_id, %charge_id, "charge belongs to another service");
            Ok(None)
        }
        Ok(Some(found)) => Ok(Some(found)),
        Ok(None) => {
            tracing::info!(%event_id, %charge_id, "charge has no checkout session");
            Ok(None)
        }
        Err(e) if !e.retryable() => {
            tracing::error!(%event_id, %charge_id, "charge lookup failed: {e}");
            hold(
                state,
                &format!("charge:{charge_id}"),
                "lookup_failed:permanent",
            )
            .await?;
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Records a hold with the operator notice.
pub(crate) async fn hold(state: &AppState, id: &str, reason: &str) -> Result<()> {
    let notify = outbox::hold_row(state, id, reason);
    if db::insert_hold(&state.db, id, reason, notify).await? {
        tracing::warn!(%reason, "hold recorded");
    }
    Ok(())
}

async fn revoke(
    state: &AppState,
    event_id: &str,
    session: &SessionInfo,
    reason: &str,
) -> Result<()> {
    // Hold first: a concurrent release then refuses, or has stored the licence the insert below revokes.
    if db::mark_hold_reversed(&state.db, &session.id, reason).await? {
        tracing::warn!(%event_id, %reason, "held sale reversed, release refused");
    } else if db::insert_hold(&state.db, &session.id, reason, None).await? {
        tracing::warn!(%event_id, %reason, "licence revoked or reversal recorded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fulfill::tests::{paid_session, test_state};
    use crate::state::Lookup;

    const SESSION: &str = "cs_paid_rev";

    fn charge(id: &str, amount_refunded: i64) -> ChargeInfo {
        ChargeInfo {
            id: id.into(),
            payment_intent_id: Some("pi_1".into()),
            amount: 49900,
            amount_refunded,
            currency: "eur".into(),
            refunded: amount_refunded == 49900,
            disputed: false,
        }
    }

    fn stub<'a>(
        _: &'a AppState,
        charge_id: &'a str,
    ) -> Lookup<'a, Option<(SessionInfo, ChargeInfo)>> {
        let mut session = paid_session(SESSION, "acme-business-annual");
        let found = match charge_id {
            "ch_full" => Some(charge(charge_id, 49900)),
            "ch_partial" => Some(charge(charge_id, 10000)),
            "ch_formshive" => {
                session.id = "cs_formshive_rev".into();
                session
                    .metadata
                    .insert("service".into(), "formshive".into());
                Some(charge(charge_id, 49900))
            }
            _ => None,
        }
        .map(|c| (session, c));
        Box::pin(async move { Ok(found) })
    }

    async fn state_with_licence() -> AppState {
        let mut state = test_state().await;
        state.lookups.charge = stub;
        let s = paid_session(SESSION, "acme-business-annual");
        assert!(matches!(
            fulfill::fulfill_paid_session(&state, &s).await.unwrap(),
            FulfillOutcome::Ready { .. }
        ));
        state
    }

    fn refunded(charge_id: &str) -> Event {
        Event::Refunded {
            event_id: "evt_r".into(),
            charge_id: charge_id.into(),
            payment_intent_id: Some("pi_1".into()),
        }
    }

    fn disputed(charge_id: &str) -> Event {
        Event::Disputed {
            event_id: "evt_d".into(),
            dispute_id: "dp_1".into(),
            charge_id: charge_id.into(),
            payment_intent_id: Some("pi_1".into()),
            amount: 49900,
            currency: "eur".into(),
        }
    }

    async fn revoked_reason(state: &AppState) -> Option<String> {
        db::get_by_session(&state.db, SESSION)
            .await
            .unwrap()
            .unwrap()
            .revoked_reason
    }

    #[tokio::test]
    async fn full_refund_revokes_once() {
        let state = state_with_licence().await;
        handle(&state, refunded("ch_full")).await.unwrap();
        let first = db::get_by_session(&state.db, SESSION)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.revoked_reason.as_deref(), Some("refund:ch_full"));
        handle(&state, refunded("ch_full")).await.unwrap();
        handle(&state, disputed("ch_full")).await.unwrap();
        let again = db::get_by_session(&state.db, SESSION)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.revoked_reason.as_deref(), Some("refund:ch_full"));
        assert_eq!(again.revoked_at, first.revoked_at);
        assert_eq!(db::list_revoked(&state.db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn partial_refund_does_not_revoke() {
        let state = state_with_licence().await;
        handle(&state, refunded("ch_partial")).await.unwrap();
        assert_eq!(revoked_reason(&state).await, None);
    }

    #[tokio::test]
    async fn dispute_revokes_once() {
        let state = state_with_licence().await;
        handle(&state, disputed("ch_partial")).await.unwrap();
        assert_eq!(
            revoked_reason(&state).await.as_deref(),
            Some("dispute:dp_1")
        );
        handle(&state, disputed("ch_partial")).await.unwrap();
        assert_eq!(db::list_revoked(&state.db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn charge_without_a_session_is_acknowledged() {
        let state = state_with_licence().await;
        handle(&state, refunded("ch_other")).await.unwrap();
        handle(&state, disputed("ch_other")).await.unwrap();
        assert_eq!(revoked_reason(&state).await, None);
        assert!(db::list_holds(&state.db, true).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn another_services_charge_records_nothing() {
        let state = state_with_licence().await;
        handle(&state, refunded("ch_formshive")).await.unwrap();
        handle(&state, disputed("ch_formshive")).await.unwrap();
        assert!(db::list_holds(&state.db, true).await.unwrap().is_empty());
        assert_eq!(revoked_reason(&state).await, None);
        assert!(db::get_by_session(&state.db, "cs_formshive_rev")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn reversal_before_any_row_holds_the_session() {
        for (event, reason) in [
            (refunded("ch_full"), "refund:ch_full"),
            (disputed("ch_partial"), "dispute:dp_1"),
        ] {
            let mut state = test_state().await;
            state.lookups.charge = stub;
            handle(&state, event).await.unwrap();
            assert!(db::list_revoked(&state.db).await.unwrap().is_empty());

            let s = paid_session(SESSION, "acme-business-annual");
            assert!(matches!(
                fulfill::fulfill_paid_session(&state, &s).await.unwrap(),
                FulfillOutcome::Held
            ));
            assert!(db::get_by_session(&state.db, SESSION)
                .await
                .unwrap()
                .is_none());
            let hold = db::open_hold(&state.db, SESSION).await.unwrap().unwrap();
            assert_eq!(hold.reason, reason);

            let err = crate::cli::release_paid(&state, &s).await.unwrap_err();
            assert!(err.to_string().contains(reason), "{err:#}");
            assert!(db::get_by_session(&state.db, SESSION)
                .await
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test]
    async fn reversal_while_held_blocks_release() {
        for (event, reason) in [
            (refunded("ch_full"), "refund:ch_full"),
            (disputed("ch_partial"), "dispute:dp_1"),
        ] {
            let mut state = test_state().await;
            state.lookups.charge = stub;
            let mut s = paid_session(SESSION, "acme-business-annual");
            s.amount_total = Some(1);
            assert!(matches!(
                fulfill::fulfill_paid_session(&state, &s).await.unwrap(),
                FulfillOutcome::Held
            ));
            handle(&state, event.clone()).await.unwrap();
            handle(&state, refunded("ch_full")).await.unwrap();
            let hold = db::open_hold(&state.db, SESSION).await.unwrap().unwrap();
            assert_eq!(hold.reason, reason);

            let err = crate::cli::release_paid(&state, &s).await.unwrap_err();
            assert!(err.to_string().contains(reason), "{err:#}");
            assert!(db::get_by_session(&state.db, SESSION)
                .await
                .unwrap()
                .is_none());
            let rec = fulfill::prepare(&state, &s, true).unwrap();
            assert!(!db::release_hold(&state.db, &rec, &[]).await.unwrap());
        }
    }

    #[tokio::test]
    async fn stripe_unreachable_on_refund_is_retryable() {
        let state = test_state().await;
        assert!(handle(&state, refunded("ch_full")).await.is_err());
    }
}
