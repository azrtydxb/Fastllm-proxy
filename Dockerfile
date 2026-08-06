# syntax=docker/dockerfile:1.7
#
# Built natively on the arm64 runners rather than under QEMU: the kw cluster is
# arm64 throughout, and emulating a Rust release build with LTO is far slower
# than it is worth.

FROM rust:1-bookworm AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
# The root manifest declares a workspace, so cargo insists on loading every
# member before it will build anything — omitting this fails the build outright
# rather than merely skipping the benchmarks. `default-members` still keeps
# them out of the release build; only their manifest has to be readable.
COPY bench ./bench

# Cache mounts are not part of the resulting layer, so the binary has to be
# copied out of the target directory inside the same step that produced it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked \
 && cp target/release/fastllm-proxy /usr/local/bin/fastllm-proxy

FROM debian:bookworm-slim

# ca-certificates is what makes an `https://` api_base work: the proxy prefers
# the system root store and only falls back to its bundled copy.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 65532 --user-group --no-create-home --shell /usr/sbin/nologin fastllm

COPY --from=build /usr/local/bin/fastllm-proxy /usr/local/bin/fastllm-proxy

USER 65532:65532
EXPOSE 4000

# Loopback is the right default for a binary someone runs on a workstation, and
# the wrong one for a container, where the kubelet has to reach it.
ENV FASTLLM_HOST=0.0.0.0 \
    FASTLLM_PORT=4000 \
    FASTLLM_CONFIG=/etc/fastllm/config.yaml

ENTRYPOINT ["/usr/local/bin/fastllm-proxy"]
