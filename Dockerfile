FROM rust:1-bookworm AS builder

WORKDIR /usr/src/tplink-plug-exporter
COPY Cargo.toml Cargo.lock ./

# Create dummy source to cache dependency compilation
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Copy real source code and rebuild only the app
COPY src ./src
RUN touch src/main.rs && cargo build --release

# Final runtime image
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/tplink-plug-exporter/target/release/tplink-plug-exporter /usr/local/bin/tplink-plug-exporter

EXPOSE 9233
ENTRYPOINT ["/usr/local/bin/tplink-plug-exporter"]

