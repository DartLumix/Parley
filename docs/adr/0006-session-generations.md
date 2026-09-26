# ADR 0006 — Registry session generations

## Status
Accepted (Phase 6c)

## Context
The session registry is keyed by user_id. A second login by the same
user replaces the first entry — but the FIRST session's cleanup then
unconditionally unregistered the user and left their voice channel,
booting the NEW session offline. Classic stale-cleanup race found by
the external audit.

## Decision
`register()` returns a monotonically increasing generation.
Cleanup uses `unregister_if(user_id, generation)` — atomic
check-and-remove under the registry lock — and only the winner runs
voice-leave + departure broadcast. `is_current` guards
JoinVoice/LeaveVoice from a replaced session's stray messages.

## Consequences
- "Latest login wins" is now clean: the successor keeps voice
  membership, the predecessor's death is a no-op.
- Voice membership survives reconnects (a feature: token relogin
  doesn't drop you from the channel).
- Known residual micro-race (documented, accepted): a register landing
  between unregister_if and voice.leave would be booted from voice;
  coupling the registry and voice locks isn't worth it.
- Incident data: HANDOFF §8.
