# Continuous Profiling

Manual performance investigation doesn't scale — this module makes profiling
an always-on background activity instead of something engineers do reactively
after users complain.

## Recording samples

`ProfilerState` (`backend/src/profiler.rs`) holds an in-memory ring buffer
(capped at 5,000 samples) of `ProfileSample { operation, stack, duration_ms,
recorded_at }`.

Wrap any operation you want profiled with `profile_operation`:

```rust
let vault = profile_operation(&state.profiler_state, "vault.create", &["handler", "db", "insert"], || async {
    db.create_vault(&input).await
}).await;
```

This is the continuous profiling hook — every call records a sample with no
manual toggling required.

## API

- `GET /admin/profiler/samples` — recent raw samples (JSON)
- `GET /admin/profiler/flamegraph` — folded-stack format
  (`frame1;frame2;frame3 <weight>` per line), directly consumable by
  flame graph renderers such as `inferno-flamegraph` or Brendan Gregg's
  `flamegraph.pl`
- `POST /admin/profiler/baseline` — snapshot current per-operation average
  durations as the new baseline
- `GET /admin/profiler/regressions?threshold_pct=20` — operations whose
  current average duration exceeds the baseline by more than
  `threshold_pct` (default 20%)

## Flame graph generation

```
curl http://localhost:3000/admin/profiler/flamegraph > out.folded
flamegraph.pl out.folded > flamegraph.svg
```

Each line's weight is the cumulative milliseconds spent in that exact call
stack across all recorded samples, so wider frames represent more total time
spent, matching standard flame graph semantics.

## Performance regression detection

1. Establish a baseline after a known-good deploy: `POST /admin/profiler/baseline`.
2. Traffic continues to record samples.
3. Periodically (e.g. from CI or a cron job) call
   `GET /admin/profiler/regressions` — any operation whose average duration
   grew by more than the threshold is returned with `baseline_avg_ms`,
   `current_avg_ms`, and `percent_change`, sorted worst-first.
4. Wire this into alerting (e.g. fail a CI job or page on-call) if the
   response is non-empty.
