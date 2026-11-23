FROM rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock index.html ./
COPY src ./src
COPY crates ./crates
COPY web ./web
RUN --mount=type=secret,id=cargo_ca \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    if [ -f /run/secrets/cargo_ca ]; then export CARGO_HTTP_CAINFO=/run/secrets/cargo_ca; fi; \
    cargo build --release --locked && install -m 0755 target/release/pump /pump

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -o 10001 -g 10001 /app/data
COPY --from=build /pump /usr/local/bin/pump
WORKDIR /app
USER 10001:10001
ENV BIND_ADDR=0.0.0.0 PORT=8080 PUMP_DATA_DIR=/app/data
EXPOSE 8080
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/pump"]
