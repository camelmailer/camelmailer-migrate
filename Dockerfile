# Build the static-ish binary, then ship it on a slim Debian runtime with
# just the CA certificates it needs to reach the target and the database over
# TLS. Uses rustls, so no OpenSSL at runtime.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/camelmailer-migrate /usr/local/bin/camelmailer-migrate
ENTRYPOINT ["camelmailer-migrate"]
