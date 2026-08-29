# Anomaly Detection

`backend/src/anomaly_detection.rs` implements an online anomaly detector
for arbitrary numeric metrics (works well fed from the same values pushed
to `/metrics/custom`, see `docs/custom-dashboards.md`).

## How it works

- **Baseline learning**: a running mean and variance is maintained per
  metric using Welford's algorithm (`AnomalyStore::observe`), so no
  historical buffer needs to be stored.
- **Detection algorithm**: each new value is scored against the *previous*
  baseline as a z-score (`(value - mean) / std_dev`). Anything with
  `|z| >= 3.0` is flagged.
- **False positive filtering**:
  - The first `MIN_SAMPLES_FOR_DETECTION` (5) points for a metric only
    train the baseline - they can never generate an alert.
  - A 60 second cooldown per metric prevents a single sustained spike from
    generating dozens of duplicate alerts.
- **Alert generation**: alerts are labeled `warning` (`|z| >= 3`) or
  `critical` (`|z| >= 6`) and kept in memory for retrieval.

## API

```
POST /anomaly/observe
{"metric": "checkout_latency_ms", "value": 128.4}
=> {"alert": null}                     # normal, or still learning baseline
=> {"alert": {"id": ..., "z_score": 4.2, "severity": "warning", ...}}

GET /anomaly/alerts                    # all alerts generated so far
GET /anomaly/baseline/checkout_latency_ms   # current learned mean/count
```

## Tuning

The z-score threshold, minimum sample count, and cooldown window are
constants at the top of `anomaly_detection.rs`
(`DEFAULT_Z_THRESHOLD`, `MIN_SAMPLES_FOR_DETECTION`,
`ALERT_COOLDOWN_SECONDS`). Widen the threshold or cooldown for noisy
metrics to reduce false positives further.
