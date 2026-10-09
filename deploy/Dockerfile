# syntax=docker/dockerfile:1
# Статический musl-бинарь в distroless/static (nonroot, uid 65532). Цель — образ < 15 МБ.
FROM rust:1-alpine AS chef
RUN apk add --no-cache musl-dev && cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG SOURCE_DATE_EPOCH=0
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} \
    RUSTFLAGS="--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=cargo"
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json -p notesync-server
COPY . .
RUN cargo build --release --locked -p notesync-server --bin notesync \
 && strip target/release/notesync \
 && mkdir -p /out/data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /src/target/release/notesync /usr/local/bin/notesync
COPY --from=builder --chown=65532:65532 /out/data /data
ENV NOTESYNC_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 8085
HEALTHCHECK --interval=30s --timeout=3s CMD ["/usr/local/bin/notesync", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/notesync"]
CMD ["serve", "--listen", "0.0.0.0:8085"]
