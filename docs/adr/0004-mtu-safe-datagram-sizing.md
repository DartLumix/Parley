# ADR 0004 — MTU-safe media datagram sizing

## Status
Accepted (Phase 6e)

## Context
quinn's path-MTU black-hole detection collapses the estimate to the
1200-byte floor after sustained loss. Our video fragments were sized
to ~1199 bytes — legal while the estimate was healthy (loopback
climbs high), then rejected with `TooLarge` minutes into a netem run,
killing the share. The failure was initially misdiagnosed as
"connection closed" because the send error wasn't printed.

## Decision
Media datagrams are sized for the FLOOR, not the healthy estimate:
`MTU_SAFE_DATAGRAM_SIZE = 1150` (1200 minus QUIC packet overhead and
the DATAGRAM frame bound), fragment payload 1130. Datagrams that fit
only while the MTU estimate is healthy are forbidden. Send errors are
ALWAYS printed verbatim — never summarized as "connection closed".

## Consequences
- ~4% more fragments per frame — negligible.
- Media survives MTU-estimate collapse on any connection, including
  the server's independent forwarding legs.
- Cost of the alternative: a share that dies minutes into every
  sustained-loss event, with a misleading log line.
- Incident data: [../hd-encode-runs.md](../hd-encode-runs.md).
