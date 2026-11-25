# LabMango Router image. Build from the repository root:  docker build -t labmango-router .
FROM rust:1.91-bookworm AS build
WORKDIR /app
# Dependency layer, cached until Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
COPY api-contracts-rs ./api-contracts-rs
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Extra CA certificates (corporate TLS inspection, Microsoft endpoints): every deploy/certs/*.crt.
COPY deploy/certs/ /usr/local/share/ca-certificates/
RUN update-ca-certificates
# Shared with the nginx container (generated map, reload requests). A new Docker volume copies this ownership.
RUN mkdir -p /nginx-admin && chown 65532:65532 /nginx-admin
COPY --from=build /app/target/release/tenant-resolver /usr/local/bin/tenant-resolver
USER 65532:65532
EXPOSE 8000
HEALTHCHECK --interval=10s --timeout=4s --start-period=20s --retries=3 CMD ["tenant-resolver", "healthcheck"]
ENTRYPOINT ["tenant-resolver"]
CMD ["serve"]
