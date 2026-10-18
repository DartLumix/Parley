# ADR 0005 — Single-owner database actor

## Status
Accepted (Phase 6c)

## Context
rusqlite's `Connection` is `Send + !Sync`. The original design held it
behind `std::sync::Mutex` and locked it inside async handlers — three
problems: blocking I/O (Argon2 verifies, WAL checkpoints) on tokio
workers; lock poisoning turning any db panic into a server-wide
cascade of panics; and lock contention serializing all db work
including auth.

## Decision
The Connection moves to a dedicated OS thread (the "db actor"):
requests via tokio unbounded mpsc (`blocking_recv` on the thread),
replies via oneshot. `DbHandle` is the cloneable async surface; the
Mutex is deleted entirely.

## Consequences
- No lock, no poisoning, no blocking on workers — the three original
  problems all disappear by construction.
- All db operations serialize on one thread — fine at this project's
  scale (chat + auth + history); the earlier `spawn_blocking`-per-call
  alternative was rejected as scattered and still Mutex-bound.
- Known accepted risk: a flood of failed logins serializes on the
  actor (Argon2 ~100 ms each) and can stall db ops. Trusted-community
  threat model; revisit on any public exposure (fix shape: separate
  auth executor).
- Incident data: [../hd-encode-runs.md](../hd-encode-runs.md).
