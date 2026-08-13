# syntax=docker/dockerfile:1.7
#
# The operator ships as its own image. It has nothing in common with the proxy
# image at runtime — no UI, no classifier models, no migrations, no Postgres
# driver — and bundling them would mean every operator upgrade pulled 200MB of
# things a controller never executes.
#
#   docker build -f operator/Dockerfile -t ghcr.io/azrtydxb/fastllm-operator:v0.1.0 .
#
# Context is the repository root, not `operator/`: the crate is a workspace
# member, so its dependency versions come from the root Cargo.lock and
# building it from its own directory would resolve them again.

FROM rust:1-trixie AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY openapi.json ./openapi.json
COPY bench ./bench
COPY operator ./operator
# `rust-embed` in the proxy crate reads `web/dist/` at compile time. The
# operator does not depend on the proxy crate, but cargo still parses every
# workspace member's manifest, so the directory has to exist.
RUN mkdir -p web/dist

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p fastllm-operator --bin fastllm-operator \
 && cp target/release/fastllm-operator /usr/local/bin/fastllm-operator

# Debian rather than distroless or scratch: the binary links against the
# system libstdc++/libssl the build stage used, and matching that exactly is
# what keeps "it built" and "it starts" the same statement.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/fastllm-operator /usr/local/bin/fastllm-operator
# Matches the `runAsUser` in operator/deploy/operator.yaml. A mismatch there
# is a pod that cannot start, reported as a permissions error with no obvious
# owner.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fastllm-operator"]
