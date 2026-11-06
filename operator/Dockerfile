# syntax=docker/dockerfile:1.7
#
# The operator ships as its own image. It has nothing in common with the proxy
# image at runtime — no UI, no classifier models, no migrations, no Postgres
# driver — and bundling them would mean every operator upgrade pulled 200MB of
# things a controller never executes.
#
#   docker build -f operator/Dockerfile -t ghcr.io/azrtydxb/fastllm-operator:v0.3.6 .
#
# Context is the repository root, not `operator/`: the crate is a workspace
# member, so its dependency versions come from the root Cargo.lock and
# building it from its own directory would resolve them again.

FROM --platform=$BUILDPLATFORM rust:1-trixie AS build
ARG TARGETARCH
ARG BUILDARCH
WORKDIR /src

# Cross toolchain for $TARGETARCH (see the proxy Dockerfile): nothing extra when
# the target is the build machine's own architecture.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  gnu=x86-64-linux-gnu;  cross=amd64 ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; cross=arm64 ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    tvar=$(echo "$triple" | tr 'a-z-' 'A-Z_'); cvar=$(echo "$triple" | tr '-' '_'); \
    if [ "$TARGETARCH" = "$BUILDARCH" ]; then cc=gcc; cxx=g++; \
    else \
      apt-get update; \
      apt-get install -y --no-install-recommends "g++-$gnu" "libc6-dev-$cross-cross"; \
      rm -rf /var/lib/apt/lists/*; \
      cc="$gnu-gcc"; cxx="$gnu-g++"; \
    fi; \
    rustup target add "$triple"; \
    { echo "export RUST_TARGET=$triple"; \
      echo "export CARGO_TARGET_${tvar}_LINKER=$cc"; \
      echo "export CC_$cvar=$cc"; \
      echo "export CXX_$cvar=$cxx"; } > /xenv.sh

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
    --mount=type=cache,target=/src/target,id=fastllm-operator-target-${TARGETARCH} \
    . /xenv.sh \
 && cargo build --release --target "$RUST_TARGET" -p fastllm-operator --bin fastllm-operator \
 && cp "target/$RUST_TARGET/release/fastllm-operator" /usr/local/bin/fastllm-operator

# Distroless cc is Debian 13 like the build stage, so the binary finds the same
# libstdc++/libssl, and it has no RUN: nothing runs under emulation. It carries
# the system CA bundle.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /usr/local/bin/fastllm-operator /usr/local/bin/fastllm-operator
# Matches the `runAsUser` in operator/deploy/operator.yaml. A mismatch there
# is a pod that cannot start, reported as a permissions error with no obvious
# owner.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fastllm-operator"]
