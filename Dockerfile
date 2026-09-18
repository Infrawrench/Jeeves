FROM rust:1.98.0-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --locked --release --bin jeeves

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/jeeves /usr/local/bin/jeeves
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/jeeves"]
