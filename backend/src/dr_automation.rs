//! Disaster Recovery runbook automation hooks (#376).
//!
//! `docs/disaster-recovery-runbook.md` documents manual DR steps an operator
//! performs by hand during an incident — typing `stellar contract invoke`
//! commands under pressure is exactly the kind of task where a typo causes
//! real damage. This module wraps two of the most error-prone steps as
//! scriptable, audited API endpoints:
//!
//! - **Failover trigger** (runbook §1, Emergency Contract Pause): flips the
//!   backend into a tracked "failover active" state, opens a Sev1 incident,
//!   and records the action to a DR audit history — the automation
//!   equivalent of an operator running the pause command and telling the
//!   team what they did.
//! - **Backup restore validation** (runbook §4, Data Recovery): runs the
//!   same checksum + integrity + restore-simulation pipeline as
//!   `backup_validation.rs` through a DR-specific endpoint that logs the run
//!   to the DR action history and opens an incident on checksum mismatch.
//!
//! Triggering or resolving failover is destructive enough to warrant a
//! safety net beyond normal admin auth: both require a short-lived,
//! single-use confirmation token minted by a separate call, so one
//! accidental request — a stray retry, a copy-pasted curl command — can
//! never execute a DR action by itself. Backup-restore validation is
//! read-only and does not require one.
//!
//! # Architecture
//!
//! ```text
//! POST /admin/dr/confirmations              → prepare_confirmation
//! POST /admin/dr/failover/trigger           → trigger_failover
//! POST /admin/dr/failover/resolve           → resolve_failover
//! GET  /admin/dr/failover/status            → failover_status
//! POST /admin/dr/backup-restore/validate    → validate_backup_restore
//! GET  /admin/dr/history                    → dr_history
//! ```
//!
//! Every endpoint requires an admin API key (`audit::authorize_admin`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    audit::authorize_admin,
    backup_validation::{BackupValidator, ChecksumStatus},
    db::AppState,
    error::AppError,
    incidents::{open_incident, IncidentSeverity},
};

/// How long a confirmation token remains valid before it must be re-issued.
const CONFIRMATION_TTL_MINUTES: i64 = 5;

/// Action name a confirmation token must be minted for before
/// `trigger_failover` will accept it.
pub const FAILOVER_TRIGGER_ACTION: &str = "failover_trigger";
/// Action name a confirmation token must be minted for before
/// `resolve_failover` will accept it.
pub const FAILOVER_RESOLVE_ACTION: &str = "failover_resolve";

#[derive(Debug, Clone)]
struct PendingConfirmation {
    action: String,
    expires_at: DateTime<Utc>,
}

/// One entry in the DR automation audit trail, returned by
/// `GET /admin/dr/history` for post-incident review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrActionRecord {
    pub id: String,
    pub action: String,
    pub actor: String,
    pub reason: Option<String>,
    pub result: String,
    pub timestamp: DateTime<Utc>,
}

pub struct DrAutomationState {
    confirmations: Mutex<HashMap<String, PendingConfirmation>>,
    failover_active: Mutex<bool>,
    failover_changed_at: Mutex<Option<DateTime<Utc>>>,
    history: Mutex<Vec<DrActionRecord>>,
}

impl DrAutomationState {
    pub fn new() -> Self {
        Self {
            confirmations: Mutex::new(HashMap::new()),
            failover_active: Mutex::new(false),
            failover_changed_at: Mutex::new(None),
            history: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, action: &str, actor: &str, reason: Option<String>, result: &str) {
        let entry = DrActionRecord {
            id: Uuid::new_v4().to_string(),
            action: action.to_string(),
            actor: actor.to_string(),
            reason,
            result: result.to_string(),
            timestamp: Utc::now(),
        };
        self.history.lock().unwrap().push(entry);
    }
}

impl Default for DrAutomationState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Confirmation tokens ─────────────────────────────────────────────────────

/// Mint a short-lived, single-use confirmation token scoped to `action`.
fn create_confirmation(state: &DrAutomationState, action: &str) -> (String, DateTime<Utc>) {
    let token = Uuid::new_v4().to_string();
    let expires_at = Utc::now() + Duration::minutes(CONFIRMATION_TTL_MINUTES);
    state.confirmations.lock().unwrap().insert(
        token.clone(),
        PendingConfirmation {
            action: action.to_string(),
            expires_at,
        },
    );
    (token, expires_at)
}

/// Consume a confirmation token: it must exist, be unexpired, and have been
/// issued for exactly `expected_action`. Tokens are single-use — a
/// successful call removes it, so replaying the same request twice fails
/// the second time even within the TTL window.
fn consume_confirmation(
    state: &DrAutomationState,
    token: &str,
    expected_action: &str,
) -> Result<(), String> {
    let mut confirmations = state.confirmations.lock().unwrap();
    let Some(pending) = confirmations.remove(token) else {
        return Err("confirmation token not found or already used".to_string());
    };
    if pending.action != expected_action {
        return Err(format!(
            "confirmation token was issued for action '{}', not '{expected_action}'",
            pending.action
        ));
    }
    if Utc::now() > pending.expires_at {
        return Err("confirmation token has expired".to_string());
    }
    Ok(())
}

// ── Request / response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PrepareConfirmationRequest {
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct PrepareConfirmationResponse {
    pub confirmation_token: String,
    pub action: String,
    pub expires_at: DateTime<Utc>,
}

/// Shared body for both `failover/trigger` and `failover/resolve`.
#[derive(Debug, Deserialize)]
pub struct DrActionRequest {
    pub confirmation_token: String,
    pub actor: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct FailoverStatusResponse {
    pub failover_active: bool,
    pub last_changed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct ValidateBackupRestoreRequest {
    pub backup_id: String,
    pub data_base64: String,
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// `POST /admin/dr/confirmations` — mint a confirmation token for a
/// subsequent destructive DR action. `action` must match exactly what the
/// destructive endpoint expects (`FAILOVER_TRIGGER_ACTION` or
/// `FAILOVER_RESOLVE_ACTION`).
pub async fn prepare_confirmation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PrepareConfirmationRequest>,
) -> Result<Json<PrepareConfirmationResponse>, AppError> {
    authorize_admin(&headers)?;
    if body.action.trim().is_empty() {
        return Err(AppError::InvalidInput("action must not be empty".into()));
    }

    let (confirmation_token, expires_at) =
        create_confirmation(&state.dr_automation_state, &body.action);

    Ok(Json(PrepareConfirmationResponse {
        confirmation_token,
        action: body.action,
        expires_at,
    }))
}

/// `POST /admin/dr/failover/trigger` — runbook §1 (Emergency Contract
/// Pause) automation hook. Destructive: requires a confirmation token
/// minted for `FAILOVER_TRIGGER_ACTION`. Opens a Sev1 incident so the
/// failover is tracked the same way a manually-declared one would be.
pub async fn trigger_failover(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DrActionRequest>,
) -> Result<Json<FailoverStatusResponse>, AppError> {
    authorize_admin(&headers)?;
    consume_confirmation(
        &state.dr_automation_state,
        &body.confirmation_token,
        FAILOVER_TRIGGER_ACTION,
    )
    .map_err(AppError::InvalidInput)?;

    let now = Utc::now();
    *state.dr_automation_state.failover_active.lock().unwrap() = true;
    *state.dr_automation_state.failover_changed_at.lock().unwrap() = Some(now);

    state.dr_automation_state.record(
        FAILOVER_TRIGGER_ACTION,
        &body.actor,
        Some(body.reason.clone()),
        "executed",
    );

    open_incident(
        &state.incident_state.store,
        "DR failover triggered",
        format!(
            "Failover was triggered by {} via DR automation: {}",
            body.actor, body.reason
        ),
        IncidentSeverity::Sev1,
    );

    tracing::warn!(actor = %body.actor, reason = %body.reason, "DR failover triggered via automation");

    Ok(Json(FailoverStatusResponse {
        failover_active: true,
        last_changed_at: Some(now),
    }))
}

/// `POST /admin/dr/failover/resolve` — clears failover mode once the root
/// cause is resolved. Resuming normal operation prematurely risks
/// re-exposing whatever triggered the failover, so this is confirmation-
/// gated the same way triggering is.
pub async fn resolve_failover(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DrActionRequest>,
) -> Result<Json<FailoverStatusResponse>, AppError> {
    authorize_admin(&headers)?;
    consume_confirmation(
        &state.dr_automation_state,
        &body.confirmation_token,
        FAILOVER_RESOLVE_ACTION,
    )
    .map_err(AppError::InvalidInput)?;

    let now = Utc::now();
    *state.dr_automation_state.failover_active.lock().unwrap() = false;
    *state.dr_automation_state.failover_changed_at.lock().unwrap() = Some(now);

    state.dr_automation_state.record(
        FAILOVER_RESOLVE_ACTION,
        &body.actor,
        Some(body.reason.clone()),
        "executed",
    );

    tracing::warn!(actor = %body.actor, reason = %body.reason, "DR failover resolved via automation");

    Ok(Json(FailoverStatusResponse {
        failover_active: false,
        last_changed_at: Some(now),
    }))
}

/// `GET /admin/dr/failover/status` — current failover state. Read-only, so
/// no confirmation token is required (admin auth still applies).
pub async fn failover_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<FailoverStatusResponse>, AppError> {
    authorize_admin(&headers)?;
    Ok(Json(FailoverStatusResponse {
        failover_active: *state.dr_automation_state.failover_active.lock().unwrap(),
        last_changed_at: *state.dr_automation_state.failover_changed_at.lock().unwrap(),
    }))
}

/// `POST /admin/dr/backup-restore/validate` — runbook §4 (Data Recovery)
/// automation hook. Not destructive (read-only validation), so no
/// confirmation token is required. Runs the same checksum + integrity +
/// restore-simulation pipeline as `POST /admin/validate-backup`, logs the
/// run to the DR action history, and opens an incident on checksum
/// mismatch — a bad backup discovered mid-incident is itself
/// incident-worthy.
pub async fn validate_backup_restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ValidateBackupRestoreRequest>,
) -> Result<Json<crate::backup_validation::BackupValidationResult>, AppError> {
    authorize_admin(&headers)?;

    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.data_base64)
        .map_err(|e| AppError::InvalidInput(format!("invalid base64 data: {e}")))?;

    let result =
        BackupValidator::validate_backup(&state.backup_metadata_store, &body.backup_id, &data);

    state.dr_automation_state.record(
        "backup_restore_validate",
        "system",
        None,
        if result.valid { "valid" } else { "invalid" },
    );

    if matches!(result.checksum_status, ChecksumStatus::Mismatch { .. }) {
        open_incident(
            &state.incident_state.store,
            "Backup checksum mismatch during DR validation",
            format!(
                "Backup '{}' failed checksum verification during a DR restore-validation run: {}",
                result.backup_id,
                result.error.clone().unwrap_or_default()
            ),
            IncidentSeverity::Sev2,
        );
    }

    Ok(Json(result))
}

/// `GET /admin/dr/history` — chronological (oldest-first) log of every DR
/// automation action executed, for post-incident review.
pub async fn dr_history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<DrActionRecord>>, AppError> {
    authorize_admin(&headers)?;
    Ok(Json(state.dr_automation_state.history.lock().unwrap().clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_round_trip_succeeds() {
        let state = DrAutomationState::new();
        let (token, _) = create_confirmation(&state, FAILOVER_TRIGGER_ACTION);
        assert!(consume_confirmation(&state, &token, FAILOVER_TRIGGER_ACTION).is_ok());
    }

    #[test]
    fn token_is_single_use() {
        let state = DrAutomationState::new();
        let (token, _) = create_confirmation(&state, FAILOVER_TRIGGER_ACTION);
        assert!(consume_confirmation(&state, &token, FAILOVER_TRIGGER_ACTION).is_ok());
        assert!(consume_confirmation(&state, &token, FAILOVER_TRIGGER_ACTION).is_err());
    }

    #[test]
    fn token_rejected_for_wrong_action() {
        let state = DrAutomationState::new();
        let (token, _) = create_confirmation(&state, FAILOVER_TRIGGER_ACTION);
        let err = consume_confirmation(&state, &token, FAILOVER_RESOLVE_ACTION).unwrap_err();
        assert!(err.contains("issued for action"));
    }

    #[test]
    fn unknown_token_rejected() {
        let state = DrAutomationState::new();
        assert!(consume_confirmation(&state, "not-a-real-token", FAILOVER_TRIGGER_ACTION).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let state = DrAutomationState::new();
        let token = Uuid::new_v4().to_string();
        // Insert an already-expired token directly, since fast-forwarding
        // the wall clock isn't practical in a unit test.
        state.confirmations.lock().unwrap().insert(
            token.clone(),
            PendingConfirmation {
                action: FAILOVER_TRIGGER_ACTION.to_string(),
                expires_at: Utc::now() - Duration::seconds(1),
            },
        );
        let err = consume_confirmation(&state, &token, FAILOVER_TRIGGER_ACTION).unwrap_err();
        assert!(err.contains("expired"));
    }

    #[test]
    fn history_records_actions() {
        let state = DrAutomationState::new();
        state.record(FAILOVER_TRIGGER_ACTION, "alice", Some("test".to_string()), "executed");
        let history = state.history.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].actor, "alice");
        assert_eq!(history[0].action, FAILOVER_TRIGGER_ACTION);
    }

    #[test]
    fn failover_starts_inactive() {
        let state = DrAutomationState::new();
        assert!(!*state.failover_active.lock().unwrap());
        assert!(state.failover_changed_at.lock().unwrap().is_none());
    }
}
