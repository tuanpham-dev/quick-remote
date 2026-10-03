# QuickRemote rendezvous + relay.
#   docker build -f crates/qr-relay/Dockerfile -t quickremote-relay .
#   docker run --rm -p 4500:4500/udp -p 40000-40999:40000-40999/udp \
#     quickremote-relay --public-ip <this server's public IP>
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p qr-relay

FROM debian:stable-slim
COPY --from=build /src/target/release/qr-relay /usr/local/bin/qr-relay
VOLUME /var/lib/quickremote
EXPOSE 4500/udp 40000-40999/udp
ENTRYPOINT ["qr-relay", "--config-dir", "/var/lib/quickremote"]
