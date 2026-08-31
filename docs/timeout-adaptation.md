# Timeout Adaptation

Implements #127. Code lives in `backend/src/timeout_adaptation.rs`.

## Why

A single fixed timeout for every endpoint is always wrong somewhere: too
tight for a naturally slower endpoint (spurious failures under normal load),
too loose for a fast one (slow failure detection when it does degrade).
`AdaptiveTimeoutManager` derives a timeout per endpoint from its own recent
latency instead.

## Model

- **Latency histograms per endpoint** — `record_latency(endpoint, duration)`
  appends to a bounded rolling window (`window_size`, default 200 samples)
  kept per endpoint name.
- **Timeout calculation** — once an endpoint has at least `min_samples`
  observations, `current_timeout(endpoint)` computes the configured
  percentile (default P99) of the window, multiplies by a safety factor
  (default 1.5x), and clamps the result to `[min_timeout, max_timeout]`.
  Before enough samples exist, it returns `default_timeout`.
- **Dynamic adjustment** — because the window is rolling, the timeout
  naturally shifts as traffic patterns change; no separate recompute step is
  needed, it's derived fresh on every read from current samples.
- **Timeout prediction** — `predict_timeout(endpoint)` tracks a short
  history of computed percentile values and extrapolates using an
  exponential moving average (alpha = 0.3) plus the recent linear trend,
  giving an early read on "latency is creeping up" before it fully shows up
  in the reactive P99 timeout.
- **`snapshot(endpoint)`** returns a `Serialize`-able summary (sample count,
  current timeout, observed percentile, predicted timeout) suitable for a
  status endpoint or dashboard.

## Example

```rust
use ethos_protocol_backend::timeout_adaptation::{AdaptiveTimeoutManager, TimeoutAdaptationConfig};
use std::time::Duration;

let manager = AdaptiveTimeoutManager::new(TimeoutAdaptationConfig::default());

// After each call to a downstream dependency:
manager.record_latency("get_vault", observed_duration);

// Before making the next call:
let timeout = manager.current_timeout("get_vault");
```

## Convergence Characteristics

Two independently-computed values respond to load differently, and it's
worth knowing which one you're looking at:

### `current_timeout` — bounded, then stable

`current_timeout` is computed fresh on every call directly from whatever
samples currently sit in the rolling window — it is **not** itself an
exponential moving average, so it has no oscillation risk of its own. Under
steady-state (constant) latency:

- It converges in **exactly `min_samples` iterations** — the first call
  where the window has at least `min_samples` observations returns a
  timeout derived from the (now entirely steady-state) window contents.
- It then holds **perfectly stable** for as long as the input stays
  steady-state, since every sample in the window is identical and the
  percentile of a constant window never changes.

See `current_timeout_converges_within_min_samples_under_steady_state_load`
in the test suite.

### `predict_timeout` — EMA settles within its history window

`predict_timeout` extrapolates from an exponential moving average
(alpha = 0.3) over up to the last 20 recorded percentile values
(`HISTORY_CAPACITY`), plus a linear trend term. Under steady-state input,
the EMA is a fixed point (feeding it the same value repeatedly leaves it
unchanged) and the trend term goes to zero, so the prediction converges to
the steady-state latency — in practice within a couple of ms of the
mathematically-exact value due to `Duration <-> f64` conversion rounding.
See `predicted_timeout_converges_toward_steady_state_value`.

### Spike + recovery

A single latency spike immediately widens `current_timeout` (it shows up in
the percentile as soon as it's recorded — no delay). Because the window is
a fixed-size FIFO ring buffer, the spike is guaranteed to be fully evicted,
and the timeout fully recovered to its pre-spike baseline, after at most
`window_size` further steady-state samples — one full window rotation. It
cannot recover any faster (the spike stays visible to the percentile
calculation until it physically falls out of the window) and it never gets
"stuck" wide (it's not a decaying average, so there's no long tail). See
`timeout_widens_on_spike_then_recovers_once_it_ages_out_of_window`.

### Known gap

Both convergence properties above are exact given `multiplier = 1.0`.
There's no dedicated test yet asserting the *rate* of `predict_timeout`'s
EMA convergence when initialized from a very different value than the
steady-state target (i.e. how many iterations until it's within X% after a
regime change, as opposed to whether it eventually gets arbitrarily close)
— worth adding if the EMA's alpha is ever tuned.

## Benchmarking

There's no `criterion` dev-dependency in this workspace, so
`benchmark_adaptation_under_load` (in the module's test suite) is a
lightweight in-process benchmark: it feeds 5,000 samples through
`record_latency`/`current_timeout`/`predict_timeout` and asserts the whole
loop completes well under a second, as a regression guard against
accidentally making the hot path (e.g. the percentile sort) quadratic. For
a proper statistical benchmark, add `criterion` as a dev-dependency and
wrap the same calls in a `#[bench]`-style harness.
