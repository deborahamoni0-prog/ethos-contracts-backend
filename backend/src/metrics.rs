use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// Shared metrics state for the Ethos-Protocol backend.
#[derive(Default)]
pub struct Metrics {
    pub vaults_total: AtomicU64,
    pub checkins_total: AtomicU64,
    pub releases_total: AtomicU64,
    pub active_vaults: AtomicI64,
    pub request_errors_total: AtomicU64,
    pub http_requests_total: AtomicU64,
    pub contract_paused: AtomicU64,
    /// Total scheduled consensus (cache reconciliation) checks run.
    pub consensus_checks_total: AtomicU64,
    /// Total key conflicts detected across all consensus checks.
    pub consensus_conflicts_total: AtomicU64,
    /// 1 if the most recent consensus check found the cache consistent, 0 otherwise.
    pub consensus_consistent: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Render all metrics in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();

        push_counter(
            &mut out,
            "ethos_protocol_vaults_total",
            "Total vaults created",
            self.vaults_total.load(Ordering::Relaxed),
        );
        push_counter(
            &mut out,
            "ethos_protocol_checkins_total",
            "Total check-ins performed",
            self.checkins_total.load(Ordering::Relaxed),
        );
        push_counter(
            &mut out,
            "ethos_protocol_releases_total",
            "Total vault releases triggered",
            self.releases_total.load(Ordering::Relaxed),
        );
        push_gauge_i64(
            &mut out,
            "ethos_protocol_active_vaults",
            "Currently active (non-released) vaults",
            self.active_vaults.load(Ordering::Relaxed),
        );
        push_counter(
            &mut out,
            "ethos_protocol_request_errors_total",
            "Total API errors",
            self.request_errors_total.load(Ordering::Relaxed),
        );
        push_counter(
            &mut out,
            "ethos_protocol_http_requests_total",
            "Total HTTP requests",
            self.http_requests_total.load(Ordering::Relaxed),
        );
        push_gauge(
            &mut out,
            "ethos_protocol_contract_paused",
            "1 if contract is paused, 0 otherwise",
            self.contract_paused.load(Ordering::Relaxed),
        );
        push_counter(
            &mut out,
            "ethos_protocol_consensus_checks_total",
            "Total scheduled consensus reconciliation checks run",
            self.consensus_checks_total.load(Ordering::Relaxed),
        );
        push_counter(
            &mut out,
            "ethos_protocol_consensus_conflicts_total",
            "Total cache key conflicts detected by consensus checks",
            self.consensus_conflicts_total.load(Ordering::Relaxed),
        );
        push_gauge(
            &mut out,
            "ethos_protocol_consensus_consistent",
            "1 if the most recent consensus check found the cache consistent, 0 otherwise",
            self.consensus_consistent.load(Ordering::Relaxed),
        );

        out
    }
}

/// Renders a Prometheus counter line. `pub(crate)` so the load shedding
/// (#128), adaptive batching (#131) and predictive scaling (#130) modules
/// can append their own metrics in the same exposition format.
pub(crate) fn push_counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

pub(crate) fn push_gauge(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

/// Renders a Prometheus counter line with key-value labels (#364).
pub fn push_labeled_counter(
    out: &mut String,
    name: &str,
    help: &str,
    labels: &[(&str, &str)],
    value: u64,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let label_str = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    let _ = writeln!(out, "{name}{{{label_str}}} {value}");
}

/// Renders a Prometheus gauge line with key-value labels (#364).
pub fn push_labeled_gauge(
    out: &mut String,
    name: &str,
    help: &str,
    labels: &[(&str, &str)],
    value: u64,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let label_str = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    let _ = writeln!(out, "{name}{{{label_str}}} {value}");
}

/// Append bulkhead registry metrics in Prometheus text format (#364).
pub fn render_bulkhead_metrics(bulkheads: &crate::bulkhead::BulkheadRegistry) -> String {
    bulkheads.render_prometheus()
}

fn push_gauge_i64(out: &mut String, name: &str, help: &str, value: i64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_contains_all_metrics() {
        let m = Metrics::new();
        m.vaults_total.store(5, Ordering::Relaxed);
        m.checkins_total.store(10, Ordering::Relaxed);
        m.contract_paused.store(1, Ordering::Relaxed);

        let output = m.render();
        assert!(output.contains("ethos_protocol_vaults_total 5"));
        assert!(output.contains("ethos_protocol_checkins_total 10"));
        assert!(output.contains("ethos_protocol_contract_paused 1"));
    }

    #[test]
    fn test_render_contains_consensus_metrics() {
        let m = Metrics::new();
        m.consensus_checks_total.store(3, Ordering::Relaxed);
        m.consensus_conflicts_total.store(2, Ordering::Relaxed);
        m.consensus_consistent.store(0, Ordering::Relaxed);

        let output = m.render();
        assert!(output.contains("ethos_protocol_consensus_checks_total 3"));
        assert!(output.contains("ethos_protocol_consensus_conflicts_total 2"));
        assert!(output.contains("ethos_protocol_consensus_consistent 0"));
    }

    #[test]
    fn test_render_prometheus_format() {
        let m = Metrics::new();
        let output = m.render();
        assert!(output.contains("# HELP ethos_protocol_vaults_total"));
        assert!(output.contains("# TYPE ethos_protocol_vaults_total counter"));
        assert!(output.contains("# TYPE ethos_protocol_active_vaults gauge"));
    }

    #[test]
    fn test_push_labeled_metrics() {
        let mut out = String::new();
        push_labeled_counter(
            &mut out,
            "bulkhead_rejected_total",
            "Total rejected",
            &[("endpoint", "/api/vaults")],
            3,
        );
        push_labeled_gauge(
            &mut out,
            "bulkhead_active_permits",
            "Active permits",
            &[("endpoint", "/api/vaults")],
            2,
        );

        assert!(out.contains("bulkhead_rejected_total{endpoint=\"/api/vaults\"} 3"));
        assert!(out.contains("bulkhead_active_permits{endpoint=\"/api/vaults\"} 2"));
    }

    #[tokio::test]
    async fn test_render_bulkhead_metrics_integration() {
        let registry = crate::bulkhead::BulkheadRegistry::new(crate::bulkhead::BulkheadConfig {
            max_concurrent: 5,
            max_queue_size: 10,
        });

        let permit = registry.acquire("/api/keys/test").await.unwrap();
        let rendered = render_bulkhead_metrics(&registry);
        assert!(rendered.contains("bulkhead_active_permits{endpoint=\"/api/keys\"} 1"));
        assert!(rendered.contains("bulkhead_queue_depth{endpoint=\"/api/keys\"} 0"));
        drop(permit);
    }
}
