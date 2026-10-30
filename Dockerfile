FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin baocode-relay

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/baocode-relay /usr/local/bin/baocode-relay
# Certificates, the entitlements cache.
VOLUME /var/lib/baocode-relay
EXPOSE 80/tcp 443/tcp 7842/udp
ENTRYPOINT ["baocode-relay"]
CMD ["--config", "/etc/baocode-relay/relay.toml"]
