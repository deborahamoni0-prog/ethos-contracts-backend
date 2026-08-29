# Service Dependency Map

This document describes the service dependency mapping implemented in
`backend/src/dependency_map.rs`. It replaces undocumented, tribal-knowledge
dependencies with a discovered graph that supports visualization, change
detection, and impact analysis.

## Why

Without a dependency map, incident responders had no reliable way to know
which services would be affected if a given service degraded or failed.
This module builds that map from observed calls and exposes it for both
visualization and blast-radius analysis.

## Concepts

- **Edge** (`DependencyEdge`) — a directed relationship `from → to`, tagged
  with a `DependencyKind` (`synchronous_rpc`, `async_message`, `data_store`).
- **Graph** (`DependencyGraph`) — the set of all discovered edges. Supports
  `downstream_of` / `upstream_of` for direct relationships and
  `impacted_by` for the full transitive upstream closure (everything that
  would feel the effect of a service failing).
- **Change** (`DependencyChange`) — the added/removed edges between the
  graph before and after a new discovery, used to detect drift over time.

## API

### `POST /dependencies/discover`

Records one observed call between two services, typically reported by
instrumentation middleware sitting in the request path:

```json
{ "from": "api-gateway", "to": "vault-service", "kind": "synchronous_rpc" }
```

If the edge is new, it is diffed against the previous graph snapshot and the
resulting `DependencyChange` is stored in history.

### `GET /dependencies/graph`

Returns the current graph as both a raw edge list and a Graphviz DOT string
(`graph.to_dot()`) suitable for rendering with any DOT-compatible
visualization tool (e.g. `dot -Tsvg`, or a frontend graph library that
accepts DOT).

### `POST /dependencies/impact`

Given `{ "service": "vault-service" }`, returns:

- `direct_downstream` — services this service directly calls.
- `direct_upstream` — services that directly call this service.
- `transitive_impact` — every service, at any depth, that would be affected
  if this service failed (`impacted_by`).

### `GET /dependencies/changes`

Returns the history of detected graph changes, most recent last, so drift in
service topology can be reviewed over time.

## Operational notes

- The graph is currently in-memory (`Arc<Mutex<DependencyGraph>>`) and reset
  on process restart; persisting discovered edges would let the map survive
  restarts and accumulate history long-term.
- Discovery today is push-based (something must call
  `POST /dependencies/discover`); a passive discovery agent that inspects
  outgoing HTTP/gRPC calls automatically is a natural next step.
