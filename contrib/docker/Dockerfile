# Build from the repository root: docker build -f contrib/docker/Dockerfile .
FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p tablica

FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/bwilczynski/tablica"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home --shell /usr/sbin/nologin tablica \
    && mkdir -p /data \
    && chown tablica:tablica /data
# Templates and the font are embedded in the binary.
COPY --from=build /src/target/release/tablica /usr/local/bin/tablica
USER tablica
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tablica"]
CMD ["serve", "--config", "/data/config.toml"]
