FROM rust:1.99-bookworm AS builder

WORKDIR /app

# Build dependencies against a stub main so they cache across source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src target/release/deps/kube_ops_mcp*

COPY src/ src/
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/kube-ops-mcp /usr/local/bin/kube-ops-mcp
ENTRYPOINT ["kube-ops-mcp"]
