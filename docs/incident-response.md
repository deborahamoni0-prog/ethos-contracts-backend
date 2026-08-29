# Incident Response Procedures

This document describes the incident response workflow implemented in
`backend/src/incidents.rs`, replacing the previous ad-hoc, chat-coordinated
response process with structured tracking.

## Why

Without a consistent workflow, incidents were tracked informally in chat
threads, making severity, ownership, and history hard to reconstruct after
the fact. This module gives every incident a consistent lifecycle, a
severity classification, an escalation path, and an auditable timeline.

## Severity classification

| Severity | Meaning                                       | Escalation SLA |
|----------|------------------------------------------------|-----------------|
| Sev1     | Full outage or data loss — page immediately    | 10 minutes      |
| Sev2     | Major functionality degraded for many users    | 30 minutes      |
| Sev3     | Minor functionality degraded / workaround exists | 2 hours       |
| Sev4     | Cosmetic or low-impact issue                    | 8 hours         |

`IncidentSeverity::escalation_sla_minutes` encodes these thresholds;
`is_past_escalation_sla` checks whether an open incident has exceeded them.

## Lifecycle

Incidents move through `Open → Investigating → Mitigated → Resolved →
Closed` (`IncidentStatus`). Every status transition is recorded as a
timeline entry automatically.

## API

- `POST /incidents` — open an incident with `title`, `description`,
  `severity`, and optional `assigned_to`. Automatically seeds the timeline
  with an "incident opened" entry.
- `GET /incidents` — list all tracked incidents.
- `GET /incidents/:id` — fetch a single incident, including its full
  timeline.
- `POST /incidents/:id/timeline` — append a free-form timeline entry
  (`actor`, `note`) — used for investigation notes, mitigations applied, etc.
- `POST /incidents/:id/status` — transition status; records a timeline entry
  describing the before/after state.
- `POST /incidents/:id/escalate` — manually escalate (e.g. once
  `is_past_escalation_sla` returns true), incrementing `escalation_level`
  and logging the reason.

## Escalation workflow

Each incident tracks an `escalation_level`, starting at 0. Escalating
increments the level and appends a timeline entry noting the reason. A
scheduled job (see `scheduler.rs` for the existing polling pattern used
elsewhere in this backend) can call `is_past_escalation_sla` periodically
against open incidents to trigger automatic escalation before a human
notices the SLA has been missed.

## Operational notes

- Incidents are stored in-memory for now
  (`Arc<Mutex<HashMap<String, Incident>>>`); persisting to the existing
  SQLite store (`db.rs`) is a natural follow-up so history survives
  restarts.
- Escalation currently only logs via `tracing`; hooking it into the
  `oncall` module's escalation policy contacts would close the loop between
  incident severity and who gets paged.
