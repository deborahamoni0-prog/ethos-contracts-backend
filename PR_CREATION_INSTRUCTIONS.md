# Pull Request Creation Instructions

## Automated PR Creation

The branch `fix/shared-degradation-state` has been created and pushed with all changes. Due to GitHub CLI permissions, you'll need to create the PR manually.

## Branch Information

- **Branch Name**: `fix/shared-degradation-state`
- **Base Branch**: `main`
- **Commit**: `5f0497f` - "fix: move degradation state to shared SQL-backed registry"
- **Repository**: https://github.com/deborahamoni0-prog/ethos-contracts-backend

## Manual PR Creation

1. Visit: https://github.com/deborahamoni0-prog/ethos-contracts-backend/pull/new/fix/shared-degradation-state

2. Set:
   - **Base**: main
   - **Compare**: fix/shared-degradation-state

3. Fill in PR Details:

### Title
```
fix: move degradation state to shared SQL-backed registry
```

### Description
```markdown
## Summary

Move capability status registry from process-local in-memory storage to a shared SQL database, ensuring all instances in a load-balanced deployment provide consistent degradation state guidance.

## Problem

Previously, marking a capability degraded on instance A did not affect instance B's behavior. Each instance maintained its own Mutex<HashMap>, leading to contradictory guidance to clients during outages:

- Instance A: capability marked degraded → 'use fallback'
- Instance B: capability not marked → 'fully available'
- Client receives conflicting guidance depending on which instance handles the request

This defeats the purpose of graceful degradation in multi-instance deployments.

## Solution

Move DegradationState registry to SQL database, ensuring all instances read from and write to the same source of truth:

1. **Added migration #12**: Creates capability_statuses table with indexed lookups
2. **Refactored DegradationState**: Uses Arc<Db> instead of Mutex<HashMap>
3. **Database methods**: 
   - set_capability_status() - atomic upsert
   - get_capability_status() - defaults to Full if unregistered
   - list_capability_statuses() - for operator inspection
4. **HTTP handlers**: Updated to return Result types with proper error handling
5. **Routes**: Registered all 4 degradation endpoints
6. **Tests**: 8 comprehensive tests including shared store and persistence tests

## Key Benefits

✅ All instances see same state - changes are immediately visible across the fleet  
✅ Persistence - status survives process restarts  
✅ Backward compatible - HTTP API unchanged, single-instance behavior identical  
✅ Production ready - comprehensive tests, error handling, and documentation

## Testing

Added 8 unit tests:
- 6 regression tests (existing behavior preserved)
- test_two_handles_share_same_store - proves two instances sharing database see each other's changes
- test_degradation_state_persists_across_instances - proves status survives restart

All tests pass with database-backed implementation.

## Documentation

Created comprehensive documentation:
- DEGRADATION_FIX_SUMMARY.md - Technical overview
- CODE_CHANGES_SUMMARY.md - Detailed code changes with diffs
- TESTING_DEGRADATION_FIX.md - Testing guide
- IMPLEMENTATION_CHECKLIST.md - Verification checklist
- DEPLOY_README.md - Deployment guide with troubleshooting
- Updated docs/graceful-degradation.md with shared state explanation

## Changes

- backend/src/degradation.rs: Refactored DegradationState, updated all methods, rewrote tests
- backend/src/db.rs: Added migration #12, 3 database methods, AppState field
- backend/src/main.rs: Initialize DegradationState with database, register routes
- docs/graceful-degradation.md: Added shared state and persistence documentation
- Created 5 supporting documentation files

Closes #275
```

4. Click "Create pull request"

## Commit Information

**Commit Message**:
```
fix: move degradation state to shared SQL-backed registry

Move capability status registry from process-local in-memory storage to a
shared SQL database, ensuring all instances in a load-balanced deployment
see consistent degradation state.

Previously, marking a capability degraded on instance A did not affect
instance B's behavior. Each instance maintained its own HashMap, leading
to contradictory guidance to clients during outages. This fix ensures:

- All instances read from and write to the same capability_statuses table
- Changes are immediately visible across the fleet
- Status persists across process restarts
- Single-instance behavior remains unchanged

Changes:
- Add capability_statuses table (migration #12) with indexed lookups
- Refactor DegradationState to use Arc<Db> instead of Mutex<HashMap>
- Implement set_capability_status(), get_capability_status(), list_capability_statuses()
- Update HTTP handlers to return Result types with proper error handling
- Register /admin/capabilities, /capabilities/negotiate endpoints
- Add comprehensive tests proving shared store and persistence
- Update graceful-degradation.md documentation

Closes #275
```

## Files Changed

- `backend/src/degradation.rs` (79 lines modified, tests rewritten)
- `backend/src/db.rs` (120 lines added)
- `backend/src/main.rs` (10 lines modified)
- `docs/graceful-degradation.md` (40 lines added)
- `CODE_CHANGES_SUMMARY.md` (new file)
- `DEGRADATION_FIX_SUMMARY.md` (new file)
- `DEPLOY_README.md` (new file)
- `IMPLEMENTATION_CHECKLIST.md` (new file)
- `TESTING_DEGRADATION_FIX.md` (new file)

## Branch Details

```
Branch: fix/shared-degradation-state
Commit: 5f0497f9c47f6d3573f86aaa52ec7043c92ea815
1 commit ahead of main
9 files changed
2351 insertions(+)
79 deletions(-)
```

## Verification

✅ All changes staged and committed
✅ Branch pushed to remote
✅ Ready for PR creation and merge
✅ Closes issue #275
