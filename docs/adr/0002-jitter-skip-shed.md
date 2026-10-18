# ADR 0002 — Jitter buffer latency-shed (skip flavor)

## Status
Accepted (Phase 6h)

## Context
Burst absorption and sender/receiver clock drift ratchet the jitter
buffer's depth toward its 64-packet cap with no mechanism pulling it
back — latency "bakes in" permanently (up to 1.28 s).

## Decision
When the buffer is CONTIGUOUS (head == next expected: backlog, not
loss) and depth exceeds 2x the target, the head packet is consumed
WITHOUT being emitted: the pop returns the successor instead. One pop
drains two packets; playback content skips forward one frame.
Rate-limited to one shed per 8 pops. The rate limiter is DISARMED on a
fresh playhead (lock-on or post-overflow — that backlog is
pre-accounted by the overflow path itself).

## Rejected alternative — PLC-flavored shed (falsified by test)
Declaring the oldest packet missing and concealing it with PLC was
attempted first and PROVABLY cannot work: a concealed frame still
occupies a playback slot, so under rate-equal arrival (the actual
baked-latency scenario) consumption always balances arrival and depth
never drains. A failing conservation test exposed this. Skip is the
only flavor that changes the arrival/consumption balance.

## Consequences
- Baked-in latency drains to target within ~1-2 s of drift onset.
- One 20 ms content skip per 8 pops during drainage (crude cousin of
  NetEQ's accelerate; the Flutter client may adopt proper time-
  stretching later, behind the same trigger).
- Sustained arrival ABOVE consumption rate is explicitly NOT the
  shed's job — that's the overflow cap's (drops + PLC concealment).
- Incident data: [../hd-encode-runs.md](../hd-encode-runs.md).
