# Monitoring & Alerting Guide

This guide covers setting up Prometheus metrics collection and Grafana dashboards for Ethos-Protocol.

## Overview

The backend exposes a `/metrics` endpoint in Prometheus text format. Grafana scrapes this to power dashboards and alerting rules.

## Metrics Exposed

| Metric | Type | Description |
|---|---|---|
| `ethos_protocol_vaults_total` | Counter | Total vaults created |
| `ethos_protocol_checkins_total` | Counter | Total check-ins performed |
| `ethos_protocol_releases_total` | Counter | Total vault releases triggered |
| `ethos_protocol_active_vaults` | Gauge | Currently active (non-released) vaults |
| `ethos_protocol_request_errors_total` | Counter | Total API errors by endpoint |
| `ethos_protocol_contract_paused` | Gauge | 1 if contract is paused, 0 otherwise |
| `ethos_protocol_http_requests_total` | Counter | HTTP requests by method, path, status |
| `ethos_protocol_http_request_duration_seconds` | Histogram | HTTP request latency |

## Prometheus Setup

### 1. Install Prometheus

```bash
# Docker
docker run -d \
  -p 9090:9090 \
  -v $(pwd)/prometheus.yml:/etc/prometheus/prometheus.yml \
  prom/prometheus
```

### 2. Configure Scrape Target

`prometheus.yml`:

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: ethos-protocol-backend
    static_configs:
      - targets: ['localhost:8080']
    metrics_path: /metrics
```

### 3. Verify

Open `http://localhost:9090` and query `ethos_protocol_vaults_total`.

## Grafana Setup

### 1. Install Grafana

```bash
docker run -d \
  -p 3000:3000 \
  grafana/grafana
```

Default credentials: `admin` / `admin`.

### 2. Add Prometheus Data Source

1. Go to **Configuration → Data Sources → Add data source**
2. Select **Prometheus**
3. Set URL to `http://localhost:9090`
4. Click **Save & Test**

### 3. Dashboards

#### Vault Volume

```promql
# Vault creation rate (per minute)
rate(ethos_protocol_vaults_total[1m])

# Active vaults
ethos_protocol_active_vaults
```

#### Check-In Rate

```promql
# Check-ins per minute
rate(ethos_protocol_checkins_total[1m])
```

#### Error Rate

```promql
# API error rate
rate(ethos_protocol_request_errors_total[5m])

# Error ratio
rate(ethos_protocol_request_errors_total[5m])
  / rate(ethos_protocol_http_requests_total[5m])
```

## Alerting Rules

Add to `prometheus.yml` or a separate `alerts.yml`:

```yaml
groups:
  - name: ethos-protocol
    rules:
      - alert: HighErrorRate
        expr: |
          rate(ethos_protocol_request_errors_total[5m])
            / rate(ethos_protocol_http_requests_total[5m]) > 0.05
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "High API error rate (>5%)"

      - alert: BackendDown
        expr: up{job="ethos-protocol-backend"} == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Ethos-Protocol backend is unreachable"

      - alert: ContractPaused
        expr: ethos_protocol_contract_paused == 1
        for: 0m
        labels:
          severity: warning
        annotations:
          summary: "Ethos-Protocol contract is paused"
```

### Grafana Alert (UI)

1. Open a panel → **Alert** tab → **Create alert rule**
2. Set condition, e.g. `WHEN last() OF query(A, 5m, now) IS ABOVE 0.05`
3. Configure notification channel (email, Slack, PagerDuty)

## Running the Backend

```bash
cd backend
cargo run
# Metrics available at http://localhost:8080/metrics
```
