# webo — 2-stage build with cargo-chef (deps in their own layer) → slim runtime.
FROM rust:1-slim AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
COPY templates templates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json .
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
COPY templates templates
RUN cargo build --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/webo /usr/local/bin/webo
EXPOSE 5050
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fs http://localhost:5050/healthz || exit 1
# root: needs to read host /sys; webo is an observer, it exposes no shell
CMD ["webo"]
