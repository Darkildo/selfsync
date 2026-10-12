# syntax=docker/dockerfile:1
# Готовый статический бинарь из релиза server-X.Y.Z (сверяется с SHA256SUMS) в
# distroless: без исходников и сборки на сервере.
FROM alpine:3.22 AS fetch
ARG VERSION
ARG TARGETARCH
RUN apk add --no-cache curl
WORKDIR /dl
RUN set -eu; \
    test -n "$VERSION" || { echo "задайте SELFSYNC_VERSION в .env"; exit 1; }; \
    case "$TARGETARCH" in amd64) t=x86_64 ;; arm64) t=aarch64 ;; \
      *) echo "архитектура $TARGETARCH не поддерживается"; exit 1 ;; esac; \
    f="selfsync-$t-unknown-linux-musl"; \
    base="https://github.com/Darkildo/selfsync/releases/download/server-$VERSION"; \
    curl -fsSL -o "$f" "$base/$f"; \
    curl -fsSL -o SHA256SUMS "$base/SHA256SUMS"; \
    grep "  $f\$" SHA256SUMS | sha256sum -c -; \
    install -m 0755 "$f" /selfsync; \
    mkdir -p /out/data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=fetch /selfsync /usr/local/bin/selfsync
COPY --from=fetch --chown=65532:65532 /out/data /data
ENV SELFSYNC_DATA_DIR=/data
EXPOSE 8085
HEALTHCHECK --interval=30s --timeout=3s CMD ["/usr/local/bin/selfsync", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/selfsync"]
CMD ["serve", "--listen", "0.0.0.0:8085"]
