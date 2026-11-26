# syntax=docker/dockerfile:1
FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --system --create-home --uid 10001 chuck \
 && mkdir /data && chown chuck /data
COPY --from=build /src/target/release/chuck /usr/local/bin/chuck
USER chuck
ENV CHUCK_DB=/data/chuck.db \
    CHUCK_ADDR=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
# No curl in the slim image; /healthz is meant for the load balancer or orchestrator health check.
CMD ["chuck"]
