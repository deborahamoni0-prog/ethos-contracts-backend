# Custom Metrics & Grafana Dashboards

The standard Prometheus metrics in `backend/src/metrics.rs` (`/metrics`)
cover system health (vault counts, request counts, contract pause state).
For business insights - conversion funnels, per-tenant activity, feature
usage - use the custom metrics API added in `backend/src/custom_metrics.rs`.

## Recording a metric

```
POST /metrics/custom
{
  "name": "checkout_latency_ms",
  "value": 128.4,
  "tags": {"region": "eu-west-1"}
}
```

## Querying / aggregating

```
GET /metrics/custom/checkout_latency_ms/aggregate?agg=avg
GET /metrics/custom/checkout_latency_ms/aggregate?agg=p95   # sum|avg|min|max|count
GET /metrics/custom                                          # list known metric names
```

## Grafana dashboard templates

`GET /dashboards/templates` returns ready-made Grafana dashboard JSON
(schema v36):

- `vault-lifecycle` - vault creation/release rate and active vault count.
- `custom-metric-explorer` - a templated panel that plots any metric
  recorded through `/metrics/custom` by name.

Import a template in Grafana via **Dashboards → Import → Paste JSON**, or
provision it through the [Grafana provisioning
API](https://grafana.com/docs/grafana/latest/administration/provisioning/)
by writing the template JSON to a provisioning directory.

## Sharing a dashboard

```
POST /dashboards/share
{"dashboard": "vault-lifecycle", "created_by": "alice"}
=> {"token": "...", "dashboard": "vault-lifecycle", ...}

GET /dashboards/shared/<token>
```

The share token is stable even if the underlying dashboard definition
changes, so links handed out to teammates keep working.
