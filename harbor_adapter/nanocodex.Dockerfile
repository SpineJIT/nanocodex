# syntax=docker/dockerfile:1.7

FROM rust:1.97-alpine3.22 AS build

ARG TARGETARCH
ARG CARGO_PROFILE=dev
ARG TAG_NAME=dev
ARG VERGEN_GIT_SHA=unknown
ENV TAG_NAME=${TAG_NAME} \
    VERGEN_GIT_SHA=${VERGEN_GIT_SHA} \
    CARGO_TARGET_DIR=/cargo-target
WORKDIR /build
RUN apk add --no-cache cmake make musl-dev openssl-dev openssl-libs-static pkgconf

RUN --mount=type=bind,target=/build \
    --mount=type=cache,id=nanocodex-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=nanocodex-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=nanocodex-target-${TARGETARCH},target=/cargo-target \
    cargo build --locked --profile "${CARGO_PROFILE}" \
        --package nanocodex-bin --bin nanocodex && \
    mkdir /out && \
    case "${CARGO_PROFILE}" in \
        dev) artifact_dir=debug ;; \
        *) artifact_dir="${CARGO_PROFILE}" ;; \
    esac && \
    cp "/cargo-target/${artifact_dir}/nanocodex" /out/nanocodex

FROM scratch AS artifact
COPY --from=build /out/nanocodex /nanocodex

FROM alpine:3.22 AS runtime
RUN apk add --no-cache ca-certificates git
COPY --from=build /out/nanocodex /usr/local/bin/nanocodex
ENTRYPOINT ["/usr/local/bin/nanocodex"]
