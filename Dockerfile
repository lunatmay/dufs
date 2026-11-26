FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/rust-musl-cross:x86_64-musl AS amd64
COPY . .
RUN cargo install --locked --path . --root /

FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/rust-musl-cross:aarch64-musl AS arm64
COPY . .
RUN cargo install --locked --path . --root /

FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/rust-musl-cross:armv7-musleabihf AS arm
COPY . .
RUN cargo install --locked --path . --root /

FROM ${TARGETARCH} AS builder

FROM scratch
COPY --from=builder /bin/dufs /bin/dufs
STOPSIGNAL SIGINT
ENTRYPOINT ["/bin/dufs"]
