# syntax=docker/dockerfile:1.7
#
# Built natively on the arm64 runners rather than under QEMU: the kw cluster is
# arm64 throughout, and emulating a Rust release build with LTO is far slower
# than it is worth.

# The management UI's build (P4). A separate stage, not a `build.rs` that
# shells out to `npm` from the Rust build — see `src/control/ui.rs` and
# `TODO.md`'s original design note for why: that would make `cargo build`
# and `cargo test` require Node for everyone, including CI jobs that only
# want to run the Rust test suite. This stage's only job is to produce
# `web/dist/`, which the `build` stage below copies in before compiling —
# `rust-embed` reads whatever is on disk under `web/dist/` at that point,
# nothing more.
FROM node:22-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json* ./
RUN npm install
COPY web/ ./
RUN npm run build

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
# Built `web/dist/` from the `web` stage above, not the `web/` source tree —
# `rust-embed`'s derive macro (src/control/ui.rs) only ever reads this one
# directory. Without this `COPY`, the directory `rust-embed` needs to exist
# at compile time would be missing entirely inside this stage's filesystem;
# with it, the binary embeds a real, built SPA instead of degrading to the
# "UI not available" placeholder a bare `cargo build` (no prior `npm run
# build`) produces.
COPY --from=web /web/dist ./web/dist

# The fast-tier classifier model, baked in so a proxy never reaches the network
# to start serving. ~61MB of static token vectors — see `src/classifier` for why
# this tier is a lookup table rather than a transformer, and docs/classifier.md
# for the measurements that chose this model over its neighbours.
#
# The refined tier's ONNX weights are deliberately *not* baked in: they are
# 130MB, most deployments never enable a refined class, and paying that in every
# image for a feature few turn on is the wrong default. Mount them and point
# --classifier-tier2-model at the directory.
ADD --chmod=0644 https://huggingface.co/minishlab/potion-code-16M/resolve/main/model.safetensors \
    /usr/local/share/fastllm/classifier/model.safetensors
ADD --chmod=0644 https://huggingface.co/minishlab/potion-code-16M/resolve/main/tokenizer.json \
    /usr/local/share/fastllm/classifier/tokenizer.json
ADD --chmod=0644 https://huggingface.co/minishlab/potion-code-16M/resolve/main/config.json \
    /usr/local/share/fastllm/classifier/config.json

# Cache mounts are not part of the resulting layer, so the binary has to be
# copied out of the target directory inside the same step that produced it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --features "control classifier" \
 && cp target/release/fastllm-proxy /usr/local/bin/fastllm-proxy

FROM debian:bookworm-slim

# ca-certificates is what makes an `https://` api_base work: the proxy prefers
# the system root store and only falls back to its bundled copy.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 65532 --user-group --no-create-home --shell /usr/sbin/nologin fastllm

COPY --from=build /usr/local/bin/fastllm-proxy /usr/local/bin/fastllm-proxy
COPY --from=build /usr/local/share/fastllm/classifier /usr/local/share/fastllm/classifier

USER 65532:65532
EXPOSE 4000

# Loopback is the right default for a binary someone runs on a workstation, and
# the wrong one for a container, where the kubelet has to reach it.
ENV FASTLLM_HOST=0.0.0.0 \
    FASTLLM_PORT=4000 \
    FASTLLM_CONFIG=/etc/fastllm/config.yaml \
    FASTLLM_CLASSIFIER_MODEL=/usr/local/share/fastllm/classifier

ENTRYPOINT ["/usr/local/bin/fastllm-proxy"]
