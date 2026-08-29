# Health-Based Routing

## Overview

`webhook::deliver_event` sends to every matching registration regardless of
how often that endpoint has recently failed, so requests keep going to
targets that are known to be unhealthy. `health_routing` tracks a rolling
health score per delivery target (identified by URL) and uses it to skip or
de-prioritize unhealthy endpoints, with a slow-start ramp for newly
(re)registered ones.

## How Scoring Works

Each delivery attempt updates the target's `EndpointHealth` via
`record_outcome`:

- **Success-rate EWMA** — an exponential moving average
  (`alpha = 0.3`) of success (1.0) / failure (0.0) outcomes, so recent
  behavior dominates the score without a single blip causing a swing.
- **Consecutive failures** — reset to 0 on any success. Once a target hits
  `UNHEALTHY_THRESHOLD` (5) consecutive failures it is marked unhealthy and
  its weight drops to `0.0` until it succeeds again.
- **Slow start** — a target's first `SLOW_START_REQUESTS` (10) attempts ramp
  linearly from 10% to 100% weight, so a newly registered or just-recovered
  endpoint is exercised cautiously rather than immediately taking full
  traffic.

Effective `weight = slow_start_ramp × health_factor`, where `health_factor`
is the success-rate EWMA if the endpoint is healthy, or `0.0` if it isn't.

## Delivery Integration

Before `webhook::deliver_event` spawns a delivery task for a registration,
it calls `health_routing::should_route(state, &registration.url)`; if the
weight is `0.0` the delivery is skipped entirely and logged, rather than
sending a request that's very likely to fail again. Every attempt
(`attempt_delivery`) reports its outcome back via `record_outcome`.

## Inspecting Routing State

```
GET /admin/routing/health
```

Returns the per-endpoint `EndpointHealth` snapshot: EWMA success rate,
totals, consecutive failures, slow-start progress, and current weight.

```
GET /admin/routing/metrics
```

Returns an aggregate view:

```json
{
  "total_endpoints": 4,
  "healthy_endpoints": 3,
  "unhealthy_endpoints": 1,
  "endpoints_in_slow_start": 1,
  "average_success_rate": 0.87
}
```

## Testing Routing Decisions

```
POST /admin/routing/test
Content-Type: application/json

{ "endpoint": "https://hooks.example.com/primary" }
```

Returns whether that endpoint would currently receive traffic, its weight,
and a human-readable reason (no history yet / in slow-start / marked
unhealthy / healthy with an EWMA success rate), without performing any real
delivery. This is the primary tool for validating routing behavior — e.g.
after a target starts failing, poll `/admin/routing/test` to confirm it gets
routed around once it crosses `UNHEALTHY_THRESHOLD`, and that it ramps back
up through slow-start once it starts succeeding again.

## Storage

Health records live in an in-memory store (`health_routing::HealthStore`)
scoped to the process and shared with `webhook::WebhookState` via
`Arc<HealthRoutingState>`.
