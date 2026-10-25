# ---- Stage 1: shared base with the tools we need ----
# Alpine (musl-native), not a glibc image cross-compiling to musl: the
# screen-share transcode path links openh264, a C++ library. A musl-native
# g++ plus the static libstdc++ is what keeps the final binary self-contained
# for the `scratch` image below.
FROM rust:1-alpine AS chef
RUN apk add --no-cache build-base cmake musl-dev perl nasm libstdc++-dev
RUN cargo install cargo-chef
WORKDIR /app

# ---- Stage 2: compute the dependency "recipe" ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 3: compile dependencies, then the code ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# The vendored crate is a `[patch.crates-io]` target, and cargo-chef's recipe
# only carries workspace members: without this copy `cook` resolves /app/Cargo.toml
# against a `vendor/moq-vaapi` that does not exist yet and dies on the patch.
COPY vendor/ vendor/
# openh264 is C++ and links libstdc++ dynamically by default. Ask the linker
# for the static archive so the final scratch image needs no C++ runtime.
ENV RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-static-libstdc++ -C link-arg=-static-libgcc"
# audiopus_sys builds Opus 1.3, whose CMakeLists predates policy 3.5;
# Alpine's CMake 4 refuses to configure it without this floor. Harmless
# on CMake 3.x (see docs/development.md).
ENV CMAKE_POLICY_VERSION_MINIMUM=3.5
# Cook only the server package. The recipe still lists the whole workspace,
# and the Flutter bridge brings proc-macro crates (flutter_rust_bridge's
# delegate-attr) that the image never needs. The explicit --target also
# matters: without it Cargo applies RUSTFLAGS to host units too, and
# `+crt-static` makes rustc refuse the server's own proc macros
# (prost-derive). With --target, build scripts and proc macros stay on the
# host defaults while the shipped binary gets the static link.
RUN cargo chef cook -p parley-server --release --recipe-path recipe.json \
    --target x86_64-unknown-linux-musl
COPY . .
RUN cargo build --release -p parley-server --bin parley-server --bin parley-adduser \
    --target x86_64-unknown-linux-musl
RUN printf 'listen = "0.0.0.0:4443"\ndata_dir = "/data"\n' > /config.toml \
    && mkdir -p /data && touch /data/.keep

# ---- Stage 4: the image that actually ships ----
FROM scratch
LABEL org.opencontainers.image.title="Parley" \
      org.opencontainers.image.description="Self-hosted voice, chat and screen share server" \
      org.opencontainers.image.source="https://github.com/DartLumix/Parley" \
      org.opencontainers.image.licenses="MIT"
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/parley-server /parley-server
# Account management without a shell: `docker compose exec parley
# /parley-server adduser USER PASS`. Statically linked, so scratch is fine.
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/parley-adduser /parley-adduser
COPY --from=builder /config.toml /config.toml
COPY --from=builder --chown=65534:65534 /data /data
USER 65534:65534
ENV PARLEY_CONFIG=/config.toml
EXPOSE 4443/udp
ENTRYPOINT ["/parley-server"]
