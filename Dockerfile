# syntax=docker/dockerfile:1
# LabMango Router image:  docker build -t labmango-router .

FROM rust:1.91-bookworm AS build
WORKDIR /app
# Dependency layer: rebuilt only when the manifests or the lock file change.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked --bin tenant-resolver \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked --bin tenant-resolver

FROM debian:bookworm-slim
ARG VERSION=dev
LABEL org.opencontainers.image.title="labmango-router" \
      org.opencontainers.image.description="LabMango Router: routes users and agents to their department backend" \
      org.opencontainers.image.version="${VERSION}"
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Extra CA certificates (corporate TLS inspection, Microsoft endpoints): every deploy/certs/*.crt.
COPY deploy/certs/ /usr/local/share/ca-certificates/
RUN update-ca-certificates \
    && mkdir -p /nginx-admin && chown 65532:65532 /nginx-admin
COPY --from=build /app/target/release/tenant-resolver /usr/local/bin/tenant-resolver
USER 65532:65532
EXPOSE 8000
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=4s --start-period=20s --retries=3 CMD ["tenant-resolver", "healthcheck"]
ENTRYPOINT ["tenant-resolver"]
CMD ["serve"]
