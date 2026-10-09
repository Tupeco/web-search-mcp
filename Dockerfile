# Build: docker build -t web-search-mcp .
FROM rust:1.99-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# distroless/cc has glibc, libgcc and the CA certificates the TLS verifier reads.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /src/target/release/web-search-mcp /usr/local/bin/web-search-mcp
# Listen on all interfaces inside the container; publish the port on the host's
# loopback only, behind nginx.
ENV HOST=0.0.0.0 CONFIG=/config/config.toml
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/web-search-mcp"]
