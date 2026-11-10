# syntax=docker/dockerfile:1

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && mkdir /data

FROM scratch
LABEL org.opencontainers.image.title="SafeToPull agent" \
      org.opencontainers.image.description="Read-only update safety checks for the containers you run" \
      org.opencontainers.image.source="https://github.com/natloh/safetopull-agent" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /src/target/release/stp-agent /stp-agent
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
VOLUME /data
EXPOSE 8484
HEALTHCHECK --interval=60s --timeout=5s --start-period=10s CMD ["/stp-agent", "healthcheck"]
ENTRYPOINT ["/stp-agent"]
