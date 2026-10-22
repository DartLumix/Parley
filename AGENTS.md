# Agent notes

## Commits carry no AI attribution

This is a standing instruction from the repository owner: **never** add
`Co-Authored-By:` trailers, `Generated with ...` footers, or any other
tool/assistant credit to commits, tags, PR descriptions or release
notes. A commit message ends with its last line of prose. History that
already contains such trailers is cleaned, not amended going forward.

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
