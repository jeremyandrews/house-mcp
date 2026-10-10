# Built on an arm64 node for the cluster; see the README for the commands.
# Both bases are pinned; bump them on purpose and push a new date tag.

FROM docker.io/library/rust:1.99.0-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
COPY assets assets
RUN cargo build --release --locked

FROM docker.io/library/debian:trixie-20261005-slim
COPY --from=build /src/target/release/house-mcp /usr/local/bin/house-mcp
# A fixed numeric user, so the Deployment's runAsNonRoot can check it without
# a passwd entry.
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/house-mcp"]
