# ---- Stage 1: shared base with the tools we need ----
FROM rust:1 AS chef
RUN rustup target add x86_64-unknown-linux-musl
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef
WORKDIR /app

# ---- Stage 2: compute the dependency "recipe" ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 3: compile dependencies, then the code ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl -p parley-server

# ---- Stage 4: the image that actually ships ----
FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/parley-server /parley-server
USER 65534:65534
EXPOSE 443/udp
ENTRYPOINT ["/parley-server"]
