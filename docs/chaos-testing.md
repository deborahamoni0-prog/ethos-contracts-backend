# Chaos Engineering Tests

Implements #124. Code lives in `backend/src/chaos.rs`.

## Why

Resilience mechanisms (retries, circuit breakers, adaptive timeouts) are
only as trustworthy as the failure modes they've actually been exercised
against. This module provides fault injectors for the failure modes the
backend is expected to survive, plus a small harness to run a "system under
test" through them and report whether it degraded gracefully.

## Scenarios

- **Network failure** (`NetworkFailureInjector`) — fails a configurable
  fraction of calls at random, simulating a flaky dependency.
- **Latency injection** (`LatencyInjector`) — sleeps for a random duration
  in `[min, max]` per call; if the sampled delay exceeds a configured
  `timeout`, it's reported as an injected fault, mirroring how a real
  caller's own timeout would treat an overly slow dependency.
- **Network partition** (`NetworkPartitionSimulator` +
  `NetworkPartitionInjector`) — marks named nodes as unreachable; any
  simulated call where either side is partitioned fails. `partition`/`heal`
  let a test flip connectivity mid-run.
- **Resource exhaustion** (`ResourceExhaustionSimulator` +
  `ResourceExhaustionInjector`) — a finite pool (memory / connections / file
  descriptors) that can be driven to capacity with `exhaust()`, after which
  `acquire()` fails until units are released (RAII `ResourceGuard`).

## Running a scenario

All four injectors implement `FaultInjector`, so they plug into a single
`ChaosRunner`:

```rust
use ethos_protocol_backend::chaos::{ChaosRunner, NetworkFailureInjector, FaultInjector};

let injector = NetworkFailureInjector::new(0.3); // 30% of calls fail
let runner = ChaosRunner::new(&injector);

let result = runner.run(200, |injector| {
    // The "system under test": a resilient client that retries a few
    // times before giving up. Swap this for the real client/circuit
    // breaker/retry policy you want to chaos-test.
    for _ in 0..3 {
        if injector.inject().is_ok() {
            return Ok(());
        }
    }
    Err("gave up after 3 attempts".to_string())
});

assert!(result.passed());
```

`ChaosRunner::run` catches panics from the operation via
`std::panic::catch_unwind` — a scenario where the system under test panics
instead of returning an error shows up as `unhandled_panics`, which fails
`ChaosTestResult::passed()` regardless of the success/failure ratio. A
result "passes" when there were no panics and successes were at least as
frequent as failures, i.e. the retry/fallback logic actually absorbed the
injected chaos rather than just failing every time.

Aggregate multiple scenarios with `ChaosReport`:

```rust
let mut report = ChaosReport::new();
report.add(ChaosRunner::new(&network_failure_injector).run(100, resilient_op));
report.add(ChaosRunner::new(&partition_injector).run(100, resilient_op));
assert!(report.all_passed());
```

## Combined-Failure Scenarios (#370)

Individual fault injectors are useful, but real incidents are rarely a
single clean failure — they're usually several things going wrong at once.
Two scenarios exercise that directly, composing chaos primitives with the
actual reliability modules they're meant to protect rather than a
standalone fake operation:

### Cache down + one replica down → falls back, doesn't error

`cascading_cache_and_replica_failure_falls_back_gracefully` marks two
`NetworkPartitionSimulator` targets ("cache" and "replica-b") down
simultaneously, leaving a third ("primary-db") healthy, then drives a
`fallback::FallbackChain` through `fallback::cascade`. Findings:

- The chain correctly skips both down targets and resolves on the healthy
  one — `resolved_target` is `Some`, not an error, confirming
  `fallback.rs`'s cascade behavior degrades gracefully instead of failing
  the whole operation when the two highest-priority targets are both
  unavailable at once.
- `degraded` is correctly `true` (it didn't resolve on the first target),
  distinguishing "worked, but not optimally" from "fully healthy" for
  monitoring purposes.
- Healing "cache" and re-running the cascade confirms recovery: the chain
  resolves on the highest-priority target again and `degraded` flips back
  to `false`.

**Gap**: this scenario drives `fallback::cascade` directly with a
synthetic chain; it doesn't yet exercise a real call site (e.g. webhook
delivery or an RPC call) that's actually wired up to use a fallback chain
in production. Worth adding once a concrete caller adopts
`fallback::cascade` for cache/replica reads.

### Circuit breaker + bulkhead under concurrent load

`circuit_breaker_and_bulkhead_interact_under_load` spawns 20 concurrent
tasks against a shared `BulkheadRegistry` (max 3 concurrent, queue of 4)
and `CircuitBreaker` (opens after 4 consecutive failures), each task
acquiring a bulkhead permit before making a call that always fails.
Findings:

- No panics or deadlocks under the combined concurrent load (a panic in
  any spawned task would surface as a `join` error and fail the test
  immediately).
- The two mechanisms compose as expected: some calls are rejected by the
  bulkhead before ever reaching the breaker (queue full), and — the
  interesting part — once the breaker trips open, calls that *did* make it
  past the bulkhead are still fast-rejected by the breaker without
  invoking the (failing) operation again. The test asserts
  `operation_invocations < bulkhead_permits_acquired` specifically to
  isolate this: it's not enough for calls to fail overall, the breaker
  must demonstrably short-circuit some of them.
- Bulkhead accounting (`active` permits) returns to zero once every task
  completes — no permit leak under combined failure + concurrency.

**Gap**: this scenario calls `BulkheadRegistry::acquire` and
`CircuitBreaker::call` directly; it doesn't run through the actual
`bulkhead_middleware` Axum middleware layered onto real HTTP requests, so
it doesn't catch issues specific to how the two are layered in
`main.rs`'s router (ordering relative to other middleware, header
propagation, etc.). A follow-up using `tower::ServiceExt::oneshot` against
a real router (see the pattern in `backend/src/tests.rs`) would close that
gap.

## Extending

To add a new fault type, implement `FaultInjector` (`name`, `inject`,
`calls_total`, `faults_injected`) — see any of the four injectors in
`backend/src/chaos.rs` as a template — and it works with `ChaosRunner`
without further changes.

## Running the tests

```
cargo test -p ethos-protocol-backend chaos::
```

Note: `chaos.rs` relies on `std::panic::catch_unwind`, which requires the
`unwind` panic strategy. This workspace's `[profile.release]` uses
`panic = "abort"`, but that profile is not used by `cargo test` (which runs
under the `test`/`dev` profile), so the chaos test suite is unaffected.
