use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::{db::Db, models::Frequency};

/// Dependencies the background scheduler needs to run all of its periodic
/// jobs. Grouped into one struct (rather than many `run(...)` parameters)
/// since the job list has grown from "poll reminder preferences" into a
/// handful of unrelated periodic checks that each need their own slice of
/// shared state.
pub struct SchedulerContext {
    pub db: Arc<Db>,
    /// Distributed cache consensus checker (#373).
    pub consensus: Arc<crate::consensus::NodeCache>,
    /// Prometheus-style counters exposed at `/metrics`.
    pub metrics: Arc<crate::metrics::Metrics>,
    /// Shared incident store: consensus conflicts detected by the scheduled
    /// job are opened here the same way a manually-filed incident would be.
    pub incident_state: Arc<crate::incidents::IncidentState>,
}

/// Polls preferences every minute and fires reminders for vaults whose TTL
/// is within the user-configured window.
///
/// In production, replace `fetch_ttl_remaining` with a real Stellar RPC call
/// and `send_reminder` with actual email/SMS/push dispatch.
pub async fn run(ctx: SchedulerContext) {
    let SchedulerContext {
        db,
        consensus,
        metrics,
        incident_state,
    } = ctx;

    // Seed default secret rotation policies on startup.
    crate::secret_rotation::seed_default_policies(&db);

    let mut interval = tokio::time::interval(Duration::from_mins(1));
    // Track when we last ran the daily/hourly/periodic tasks.
    let mut last_daily_purge = chrono::DateTime::<Utc>::MIN_UTC;
    let mut last_rotation_check = chrono::DateTime::<Utc>::MIN_UTC;
    let mut last_consensus_check = chrono::DateTime::<Utc>::MIN_UTC;

    loop {
        interval.tick().await;
        let now = Utc::now();

        // 1) Existing reminder preferences scheduler.
        match db.all() {
            Ok(all_prefs) => {
                for prefs in all_prefs {
                    let ttl_hours = fetch_ttl_remaining(prefs.vault_id);
                    let window = prefs.hours_before_expiry;

                    let subscription = db.get_subscription(prefs.vault_id).ok().flatten();

                    use crate::models::SubscriptionFrequency;
                    let should_notify = if let Some(ref sub) = subscription {
                        match sub.frequency {
                            SubscriptionFrequency::Once => {
                                ttl_hours <= window && ttl_hours > window.saturating_sub(1)
                            }
                            SubscriptionFrequency::Daily => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24)
                            }
                            SubscriptionFrequency::Weekly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 7)
                            }
                            SubscriptionFrequency::Hourly => ttl_hours <= window,
                            SubscriptionFrequency::Monthly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 30)
                            }
                        }
                    } else {
                        match prefs.frequency {
                            Frequency::Once => {
                                ttl_hours <= window && ttl_hours > window.saturating_sub(1)
                            }
                            Frequency::Daily => ttl_hours <= window && ttl_hours.is_multiple_of(24),
                            Frequency::Weekly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 7)
                            }
                            Frequency::Hourly => ttl_hours <= window,
                            Frequency::Monthly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 30)
                            }
                        }
                    };

                    if should_notify {
                        for channel in &prefs.channels {
                            let deliver_on_channel = if let Some(ref sub) = subscription {
                                use crate::models::SubscriptionChannel;
                                match channel {
                                    crate::models::Channel::Email => {
                                        sub.channels.contains(&SubscriptionChannel::Email)
                                    }
                                    crate::models::Channel::Sms => {
                                        sub.channels.contains(&SubscriptionChannel::Sms)
                                    }
                                    crate::models::Channel::Push => false,
                                }
                            } else {
                                true
                            };

                            if deliver_on_channel {
                                send_reminder(prefs.vault_id, channel, ttl_hours);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to fetch reminder preferences");
            }
        }

        // 2) TTL insurance scheduler.
        extend_ttl_for_inactive_owners(&db);

        // 3) Data retention purge (runs at most once every 24 hours).
        if now.signed_duration_since(last_daily_purge).num_hours() >= 24 {
            crate::retention::run_purge_scheduler(&db);
            last_daily_purge = now;
        }

        // 4) Secret rotation overdue check (runs at most once every hour).
        if now.signed_duration_since(last_rotation_check).num_minutes() >= 60 {
            crate::secret_rotation::run_rotation_scheduler(&db);
            last_rotation_check = now;
        }

        // 5) Distributed cache consensus reconciliation (#373; runs at most
        //    once every 5 minutes — cache drift needs tighter reconciliation
        //    than the once-a-day/hour housekeeping jobs above).
        if now.signed_duration_since(last_consensus_check).num_minutes() >= 5 {
            run_consensus_check(&consensus, &metrics, &incident_state);
            last_consensus_check = now;
        }
    }
}

fn extend_ttl_for_inactive_owners(db: &Arc<Db>) {
    let policies = match db.all_enabled_insurance_policies() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "failed to fetch insurance policies");
            return;
        }
    };

    let now = Utc::now();

    for policy in policies {
        if !policy.enabled {
            continue;
        }
        let owner_last_active = match db.get_owner_last_active_at(policy.vault_id) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    vault_id = policy.vault_id,
                    error = %e,
                    "failed to fetch owner last active time"
                );
                continue;
            }
        };
        let Some(last_active) = owner_last_active else {
            continue;
        };

        let inactive_for = now.signed_duration_since(last_active).num_seconds();
        if inactive_for < policy.inactivity_threshold_seconds.cast_signed() {
            continue;
        }

        tracing::info!(
            vault_id = policy.vault_id,
            extension_seconds = policy.extension_seconds,
            "TTL extended by insurance due to inactivity"
        );

        if let Err(e) = db.upsert_insurance_policy(&crate::models::TtlInsurancePolicy {
            vault_id: policy.vault_id,
            extension_seconds: policy.extension_seconds,
            inactivity_threshold_seconds: policy.inactivity_threshold_seconds,
            enabled: true,
            purchased_at: policy.purchased_at,
            last_extended_at: Some(now),
        }) {
            tracing::error!(
                vault_id = policy.vault_id,
                error = %e,
                "failed to update insurance policy after TTL extension"
            );
        }
    }
}

/// Stub: returns hours remaining until vault TTL expiry.
/// Replace with a Stellar RPC call to `get_ttl_remaining`.
fn fetch_ttl_remaining(_vault_id: u64) -> u32 {
    u32::MAX
}

/// Stub: dispatches a reminder via the given channel.
fn send_reminder(vault_id: u64, channel: &crate::models::Channel, hours_left: u32) {
    tracing::info!(vault_id, ?channel, hours_left, "sending reminder");
}

// ── #81: Backup Validation Job ───────────────────────────────────────────────

/// Run the periodic backup validation job.
///
/// In a real deployment this would retrieve backup snapshots from durable
/// storage and validate each one.  Here we log a scheduled-run notice and
/// simulate a trivial no-op validation so the job framework is exercised
/// without requiring an external storage integration.
#[allow(dead_code)]
fn run_backup_validation_job() {
    use crate::backup_validation::BackupValidator;
    use chrono::Utc;

    let job_id = uuid::Uuid::new_v4().to_string();
    let scheduled_at = Utc::now();

    tracing::info!(
        job_id = %job_id,
        scheduled_at = %scheduled_at,
        "backup validation job started"
    );

    // Simulate validating a placeholder backup so the code path is exercised.
    // Replace with real backup retrieval when storage integration is ready.
    let placeholder_backups: Vec<(String, Vec<u8>)> = vec![];
    let results = BackupValidator::validate_all_backups(&placeholder_backups);

    for result in &results {
        if result.valid {
            tracing::info!(
                backup_id = %result.backup_id,
                "backup validation passed"
            );
        } else {
            tracing::warn!(
                backup_id = %result.backup_id,
                error = ?result.error,
                "backup validation failed"
            );
        }
    }

    tracing::info!(
        job_id = %job_id,
        validated = results.len(),
        "backup validation job completed"
    );
}

// ── #83: Consistency Check Job ───────────────────────────────────────────────

/// Run the periodic data consistency verification job.
pub fn run_consistency_check(db: &Arc<Db>) {
    use crate::consistency::ConsistencyChecker;

    tracing::info!("consistency check job started");

    let report = ConsistencyChecker::run_all_checks(db);

    for issue in &report.issues {
        match issue.severity {
            crate::consistency::IssueSeverity::Critical => {
                tracing::error!(
                    check = %issue.check_name,
                    affected_rows = issue.affected_rows,
                    description = %issue.description,
                    "CRITICAL consistency issue detected"
                );
            }
            crate::consistency::IssueSeverity::Error => {
                tracing::error!(
                    check = %issue.check_name,
                    affected_rows = issue.affected_rows,
                    description = %issue.description,
                    "consistency error detected"
                );
            }
            crate::consistency::IssueSeverity::Warning => {
                tracing::warn!(
                    check = %issue.check_name,
                    affected_rows = issue.affected_rows,
                    description = %issue.description,
                    "consistency warning detected"
                );
            }
        }
    }

    tracing::info!(
        total_checks = report.total_checks,
        passed = report.passed_checks,
        failed = report.failed_checks,
        "consistency check job completed"
    );
}

// ── #373: Consensus Reconciliation Job ───────────────────────────────────────

/// Run the periodic distributed-cache consensus reconciliation job.
///
/// Compares this node's local cache against the shared `InMemoryBackend` /
/// `RedisBackend` (see `consensus.rs`), publishes the result as metrics, and
/// — when conflicts are found — opens an incident so operators are notified
/// even if nobody is actively watching `/health/consensus` or `/metrics`.
fn run_consensus_check(
    consensus: &Arc<crate::consensus::NodeCache>,
    metrics: &Arc<crate::metrics::Metrics>,
    incident_state: &Arc<crate::incidents::IncidentState>,
) {
    tracing::info!("consensus reconciliation job started");

    let report = match consensus.check_and_resolve() {
        Ok(report) => report,
        Err(e) => {
            tracing::error!(error = %e, "consensus reconciliation job failed to run");
            return;
        }
    };

    metrics.consensus_checks_total.fetch_add(1, Ordering::Relaxed);
    metrics
        .consensus_conflicts_total
        .fetch_add(report.conflicts.len() as u64, Ordering::Relaxed);
    metrics
        .consensus_consistent
        .store(u64::from(report.consistent), Ordering::Relaxed);

    if report.consistent {
        tracing::info!(
            node_id = %report.node_id,
            keys_checked = report.keys_checked,
            "consensus reconciliation job completed: cache consistent"
        );
        return;
    }

    tracing::warn!(
        node_id = %report.node_id,
        conflicts = report.conflicts.len(),
        conflicts_resolved = report.conflicts_resolved,
        keys_checked = report.keys_checked,
        "consensus reconciliation job detected conflicts"
    );

    let conflicted_keys: Vec<&str> = report.conflicts.iter().map(|c| c.key.as_str()).collect();
    crate::incidents::open_incident(
        &incident_state.store,
        "Distributed cache consensus conflict detected",
        format!(
            "Node '{}' found {} conflicting key(s) between its local cache and the distributed \
             backend during scheduled reconciliation (strategy: {:?}). {} conflict(s) were \
             auto-resolved. Affected keys: {}",
            report.node_id,
            report.conflicts.len(),
            report.strategy,
            report.conflicts_resolved,
            conflicted_keys.join(", "),
        ),
        crate::incidents::IncidentSeverity::Sev3,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::{CacheBackend, CacheEntry, ConflictStrategy, InMemoryBackend, NodeCache};
    use crate::incidents::{create_incident_store, IncidentState};
    use crate::metrics::Metrics;
    use chrono::TimeZone;

    #[test]
    fn consensus_check_opens_incident_and_updates_metrics_on_conflict() {
        let backend: Arc<dyn CacheBackend> = Arc::new(InMemoryBackend::new());
        let consensus = Arc::new(NodeCache::new(
            "test-node",
            Arc::clone(&backend),
            ConflictStrategy::LastWriteWins,
        ));
        consensus.put("vault:1", "authoritative").unwrap();
        consensus.set_local_entry(CacheEntry {
            key: "vault:1".to_string(),
            value: "stale".to_string(),
            node_id: "test-node".to_string(),
            updated_at: chrono::Utc.timestamp_millis_opt(1).unwrap(),
            version: 1,
        });

        let metrics = Metrics::new();
        let incident_state = Arc::new(IncidentState {
            store: create_incident_store(),
        });

        run_consensus_check(&consensus, &metrics, &incident_state);

        assert_eq!(metrics.consensus_checks_total.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.consensus_conflicts_total.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.consensus_consistent.load(Ordering::Relaxed), 0);

        let incidents = incident_state.store.lock().unwrap();
        assert_eq!(incidents.len(), 1);
        let incident = incidents.values().next().unwrap();
        assert!(incident.description.contains("vault:1"));
    }

    #[test]
    fn consensus_check_does_not_open_incident_when_consistent() {
        let backend: Arc<dyn CacheBackend> = Arc::new(InMemoryBackend::new());
        let consensus = Arc::new(NodeCache::new(
            "test-node",
            backend,
            ConflictStrategy::LastWriteWins,
        ));
        consensus.put("vault:2", "value").unwrap();

        let metrics = Metrics::new();
        let incident_state = Arc::new(IncidentState {
            store: create_incident_store(),
        });

        run_consensus_check(&consensus, &metrics, &incident_state);

        assert_eq!(metrics.consensus_consistent.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.consensus_conflicts_total.load(Ordering::Relaxed), 0);
        assert!(incident_state.store.lock().unwrap().is_empty());
    }
}
