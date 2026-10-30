# syntax=docker/dockerfile:1
# Build the doggy-dns image, in two variants that share every stage below and
# differ only in the cargo feature set and the user the container runs as:
#
#   # userspace resolver, runs as a non-root user. `--target runtime` is not
#   # optional: an untargeted build produces the LAST stage, i.e. the root XDP
#   # image, under whatever tag you passed.
#   docker build --target runtime -t ghcr.io/<org>/doggy-dns:main .
#
#   # same binary plus the XDP fast path (feature `xdp`, eBPF program embedded)
#   docker build --build-arg FEATURES=xdp --target runtime-xdp \
#          -t ghcr.io/<org>/doggy-dns:main-xdp .
#
# The eBPF stage needs nightly + rust-src + bpf-linker, so it only runs for the
# `xdp` feature; the userspace image then costs nothing for it. Both variants
# compile `aws-lc-sys`/`ring` (rustls default features), which is why the builder
# carries a C toolchain and cmake, and why the runtime is glibc-based Debian
# rather than a static musl link.
#
# Registry mirrors are ARGs so a build behind a slow or blocked path can be
# redirected without editing this file:
#
#   --build-arg BUILDER_IMAGE=docker.m.daocloud.io/library/rust:1-slim-bookworm
#   --build-arg RUNTIME_IMAGE=docker.m.daocloud.io/library/debian:12-slim
#   --build-arg APT_MIRROR=mirrors.ustc.edu.cn
#   --build-arg CARGO_MIRROR_INDEX=sparse+https://rsproxy.cn/index/
#   --build-arg GHPROXY=https://gh-proxy.com/

ARG BUILDER_IMAGE=docker.io/library/rust:1-slim-bookworm
ARG RUNTIME_IMAGE=docker.io/library/debian:12-slim

# ----------------------------------------------------------------------------
# builder: the release binary, with or without the XDP fast path.
# ----------------------------------------------------------------------------
FROM ${BUILDER_IMAGE} AS builder

# `FEATURES` is the cargo feature list, empty for the userspace-only binary.
# `BPF_LINKER_VERSION` is pinned to what .github/workflows/xdp-ci.yml installs,
# so the image and CI load the same linker into the same verifier.
ARG FEATURES=
ARG BPF_LINKER_VERSION=v0.12.0
ARG CARGO_MIRROR_INDEX=
ARG GHPROXY=
# Set by Docker BuildKit; podman/buildah leaves it empty, so the build falls back
# to the machine's own `uname -m`. Either spelling is accepted (`amd64`/`x86_64`,
# `arm64`/`aarch64`) because the two conventions come from the two builders.
# bpf-linker ships one prebuilt binary per architecture.
ARG TARGETARCH
# Extra flags for the release build. The workspace's `profile.release` uses fat
# LTO and one codegen unit, which needs more memory than a small local VM has;
# `--build-arg CARGO_EXTRA_ARGS='--config profile.release.lto=false -j 1'`
# builds the same code with a link a 2 GiB host survives. CI leaves it empty.
ARG CARGO_EXTRA_ARGS=
# Debian mirror *host* to install from, e.g. `mirrors.ustc.edu.cn`. Empty means
# the image's own `deb.debian.org`. Only the hostname is rewritten, so the
# `/debian` and `/debian-security` paths the base images already use stay valid;
# bookworm keeps its URIs in the deb822 `.sources` file, so both layouts are
# handled.
ARG APT_MIRROR=
# rustup's download host, e.g. `https://rsproxy.cn`. `rust-toolchain.toml` pins
# `stable` (and `xdp/rust-toolchain.toml` pins `nightly` + `rust-src`), so every
# build downloads a toolchain from `static.rust-lang.org` before it compiles
# anything — on a network where that host is slow, the download alone takes tens
# of minutes; a mirror makes it a minute. Empty means rustup's own default.
ARG RUSTUP_MIRROR=

RUN if [ -n "${APT_MIRROR}" ]; then \
        for f in /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources; do \
            [ -f "$f" ] && sed -i -E \
                "s|(https?://)deb\.debian\.org|\1${APT_MIRROR}|g; \
                 s|(https?://)security\.debian\.org|\1${APT_MIRROR}|g" "$f"; \
        done; \
    fi

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        libclang-dev \
        perl \
        pkg-config \
        zstd \
    && rm -rf /var/lib/apt/lists/*

# A crates.io mirror is written to the cargo home the `rust` image already uses,
# so both the workspace build and the nested eBPF build pick it up.
RUN if [ -n "${CARGO_MIRROR_INDEX}" ]; then \
        mkdir -p /usr/local/cargo && \
        printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
            "${CARGO_MIRROR_INDEX}" > /usr/local/cargo/config.toml; \
    fi

WORKDIR /src
COPY . /src

# Build the eBPF program first when the fast path is asked for: `build.rs` looks
# for this exact artifact and only invokes cargo itself when it is missing or
# stale, so doing it here keeps the nightly toolchain, bpf-linker and the
# `-Z build-std` flags in one place.
RUN set -eux; \
    if [ -n "${RUSTUP_MIRROR}" ]; then \
        export RUSTUP_DIST_SERVER="${RUSTUP_MIRROR}" RUSTUP_UPDATE_ROOT="${RUSTUP_MIRROR}/rustup"; \
    fi; \
    if [ -n "${FEATURES}" ]; then \
        rustup toolchain install nightly --profile minimal --component rust-src; \
        arch="${TARGETARCH:-$(uname -m)}"; \
        case "${arch}" in \
            amd64|x86_64) linker_arch=x86_64 ;; \
            arm64|aarch64) linker_arch=aarch64 ;; \
            *) echo "unsupported architecture for bpf-linker: ${arch}" && exit 1 ;; \
        esac; \
        # 26 MB, and a slow or proxying path can stall partway. The archive goes
        # through a file rather than a pipe so a failed curl fails this layer
        # instead of feeding a truncated body to `tar`, and the version check is
        # the backstop that the binary actually extracted and runs.
        curl -sSL --retry 5 --retry-all-errors --retry-delay 3 --max-time 900 \
            -o /tmp/bpf-linker.tar.zst \
            "${GHPROXY}https://github.com/aya-rs/bpf-linker/releases/download/${BPF_LINKER_VERSION}/bpf-linker-${linker_arch}-unknown-linux-musl.tar.zst"; \
        tar -xI zstd -C /usr/local/bin -f /tmp/bpf-linker.tar.zst; \
        rm -f /tmp/bpf-linker.tar.zst; \
        bpf-linker --version; \
        cd /src/xdp; \
        cargo build -Z build-std=core,alloc --release --target bpfel-unknown-none; \
        cd /src; \
    fi; \
    if [ -n "${FEATURES}" ]; then \
        cargo build --release --features "${FEATURES}" ${CARGO_EXTRA_ARGS}; \
    else \
        cargo build --release ${CARGO_EXTRA_ARGS}; \
    fi

# ----------------------------------------------------------------------------
# runtime: Debian slim + the CA bundle the forward/Nacos paths need.
# ----------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime-base

ARG APT_MIRROR=
# The repository this image belongs to, published as the OCI `source`
# annotation. It is what GitHub uses to link the ghcr package to the repository
# when the image is pushed from a command line (a workflow push links it on its
# own), and what the package page shows. Override it if you publish a fork
# elsewhere.
ARG SOURCE_URL=https://github.com/opc-source/doggy-dns

LABEL org.opencontainers.image.source="${SOURCE_URL}"

RUN if [ -n "${APT_MIRROR}" ]; then \
        for f in /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources; do \
            [ -f "$f" ] && sed -i -E \
                "s|(https?://)deb\.debian\.org|\1${APT_MIRROR}|g; \
                 s|(https?://)security\.debian\.org|\1${APT_MIRROR}|g" "$f"; \
        done; \
    fi

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/doggy-dns /usr/local/bin/doggy-dns

# The binary's default config path is `config/doggy-dns.toml` relative to the
# working directory, and its first argument may name another file. This baked
# default is the smallest forward-only config that answers DNS and exposes the
# probes — `config/doggy-dns.toml` in the repository is a development sample and
# would not even start here (its OTLP block needs ${OPENOBSERVE_PASSWORD}). A
# deployment mounts its own file over it; see examples/deploy/.
COPY examples/deploy/container-default.toml /app/config/doggy-dns.toml
WORKDIR /app
EXPOSE 53/udp 53/tcp
ENTRYPOINT ["/usr/local/bin/doggy-dns"]

# Userspace variant: unprivileged, and answering on port 53 is granted through
# `NET_BIND_SERVICE` in the pod spec rather than by running as root.
FROM runtime-base AS runtime

RUN useradd --system --create-home --uid 65532 --user-group \
        --shell /usr/sbin/nologin doggy
USER 65532:65532

# XDP variant: loading and attaching an eBPF program, and pinning the cache map
# into the host's bpffs, need root plus the capabilities in the pod spec. It
# also has to be the process that survives a restart on the pinned map, so the
# image stays single-process and privileged.
FROM runtime-base AS runtime-xdp

USER root
