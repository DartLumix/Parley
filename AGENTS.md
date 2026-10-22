# Agent notes

## Commits carry no AI attribution

This is a standing instruction from the repository owner: **never** add
`Co-Authored-By:` trailers, `Generated with ...` footers, or any other
tool/assistant credit to commits, tags, PR descriptions or release
notes. A commit message ends with its last line of prose. History that
already contains such trailers is cleaned, not amended going forward.

## Cross-component upgrades

Components ship separately (server, Linux client, Windows client, the
Flutter bridge, packaging, the deploy kit) but share a wire protocol.
Whenever a change in one forces a change in another — protocol or
handshake version, capability bits, the feature sets in the workflows,
files one side reads from the other — say so in the reply: which
component, what exactly, and whether the two can ship independently.
Silence means nothing else has to move.

## Before committing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd app && flutter analyze && flutter test)
```

Shipping-feature changes (`audio-device`, `aec`, `video`,
`screen-capture`, `hw-encode`) also need the feature builds and tests in
`.github/workflows/ci.yml` — including the `windows` job, which builds
the reduced `audio-device,video` set. Container-facing changes need
`tools/e2e-container.sh`.

Build dependencies and the AEC/abseil caveats are in
[docs/development.md](docs/development.md); releases are tag-driven and
described in [docs/release.md](docs/release.md).
