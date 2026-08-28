# Circuit Breaker Observability

Implements #126. Code lives in `backend/src/circuit_breaker.rs`.

## Why

The backend relies on circuit breakers to protect against cascading
failures when a downstream dependency (database, Redis, an external
webhook target, etc.) starts failing. Without visibility into a breaker's
state, an operator has no way to tell "is this endpoint slow because the
dependency is degraded, or because the breaker itself is stuck open?" This
module adds the metrics, events, and alerts needed to answer that.

## Model

`CircuitBreaker` implements the standard three-state machine:

```
Closed --(failure_threshold consecutive failures)--> Open
Open   --(open_duration elapses)--> HalfOpen
HalfOpen --(success_threshold consecutive successes)--> Closed
HalfOpen --(any failure)--> Open
```

Wrap a fallible call with `breaker.call(|| do_the_thing())`. The breaker
either runs the closure and records the outcome, or immediately returns
`CircuitBreakerError::Rejected` without invoking it.

### Half-Open Request Capping (#363)

To prevent a flood of concurrent trial requests from overwhelming recovering downstream dependencies, `CircuitBreakerConfig` includes:

- `half_open_max_requests` (default: 1): Maximum number of concurrent probe requests permitted through the breaker while in the `HalfOpen` state.
- Excess concurrent requests arriving while active half-open probe slots are saturated are rejected with `CircuitBreakerError::Rejected` and increment `calls_rejected_total`.
- Once `success_threshold` consecutive successful probes complete, the breaker closes and permits normal traffic. If any probe fails, the breaker transitions immediately back to `Open`.

## Observability surface

- **State metrics** — `CircuitBreaker::render_metrics()` emits Prometheus
  text exposition format (`circuit_breaker_state`,
  `circuit_breaker_calls_allowed_total`, `circuit_breaker_calls_rejected_total`,
  `circuit_breaker_failures_total`, `circuit_breaker_successes_total`,
  `circuit_breaker_state_transitions_total`), each labeled with the
  breaker's name. `CircuitBreakerRegistry::render_metrics()` aggregates
  every registered breaker for a single `/metrics` scrape.
- **State change events** — every transition appends a `StateChangeEvent`
  (from, to, timestamp, reason) to a bounded ring buffer
  (`CircuitBreaker::events()`, capped at 200 entries) for debugging "when did
  this actually trip?"
- **State visualization** — `CircuitBreaker::render_dashboard()` produces a
  one-line human-readable summary; `CircuitBreaker::to_mermaid_diagram()`
  produces a Mermaid `stateDiagram-v2` with the current state marked, which
  renders directly in GitHub/most Markdown viewers or an internal wiki.
- **State alerts** — `CircuitBreaker::check_alerts()` (and
  `CircuitBreakerRegistry::check_alerts()` across all breakers) returns
  `CircuitAlert`s for: a breaker open longer than 4x its configured
  `open_duration` (critical), a breaker open at all (warning), and a
  breaker rejecting more than half its recent call volume (critical).

## Wiring it up

Construct one breaker per protected dependency (e.g. `"redis"`,
`"webhook-delivery"`) via a shared `CircuitBreakerRegistry`, and poll
`registry.render_metrics()` / `registry.check_alerts()` from the existing
`/health` or a new `/metrics` route alongside `Metrics::render()`.
