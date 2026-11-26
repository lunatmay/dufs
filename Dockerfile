FROM ghcr.io/rust-cross/rust-musl-cross:x86_64-musl AS builder
COPY . .
RUN cargo install --locked --path . --root /

FROM scratch
COPY --from=builder /bin/dufs /bin/dufs
STOPSIGNAL SIGINT
ENTRYPOINT ["/bin/dufs"]
