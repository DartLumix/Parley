# ADR 0003 — Quinn version pinning and vetting policy

## Status
Accepted (Phase 6e)

## Context
Two quinn-proto defects were caught in production-style testing:
- 0.11.16 SILENTLY CORRUPTS datagram bursts (reorder + loss on an
  uncongested loopback; 7/10 isolated test failures, three symptom
  modes). A routine bump+test would likely have shipped it.
- <= 0.11.17 panics ("datagrams.outgoing.payload_bytes desynchronized")
  in the outgoing drop-oldest path under send-buffer saturation.

## Decision
`quinn-proto` moves ONLY through a vetting gate, never on routine
`cargo update`:
1. `cargo test --workspace`
2. isolated run of `video_fragments_routed_between_members` x10
   (the detector that exposed .16)
3. `parley-loadtest --senders 10` (saturation exposure)
4. one netem cycle (10% / 40 ms, video ladder + voice)

The quinn FACADE may ride normal updates (it owns endpoint/driver
glue; still earns tests + loadtest). 0.11.16 is blacklisted
permanently.

## Consequences
- ~15 min of machine time per quinn-proto adoption — cheap insurance
  against silent-corruption classes that unit tests cannot see.
- Both defects to be reported upstream; re-vet on any quinn-proto
  release claiming a datagram-accounting fix.
- Incident data + the .16 blacklist evidence: HANDOFF §8.
