# The server, as a small image with no build tooling left in it.
#
# Only the server and the operator CLI are built here: mounting is the FUSE
# client's job and it belongs on a host with /dev/fuse, not in this image.
FROM rust:1-slim AS build

# rustls throughout, so there is no OpenSSL to link against and nothing to
# install beyond a C toolchain for the few crates that need one.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release -p dcfs-server -p dcfs-cli

FROM debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 dcfs

COPY --from=build /src/target/release/dcfs-server /usr/local/bin/
COPY --from=build /src/target/release/dcfs-cli /usr/local/bin/dcfs

# Where chunks go when the local object store is in use. A volume in compose,
# so it survives the container.
RUN mkdir -p /var/lib/dcfs && chown dcfs:dcfs /var/lib/dcfs
USER dcfs
ENV OBJECT_STORE_PATH=/var/lib/dcfs \
    SERVER_ADDR=0.0.0.0:8080
EXPOSE 8080

# Says the process is up and serving. It does not reach the database, so a
# server that is running with a database it cannot talk to still reads as
# healthy; compose waits on PostgreSQL's own healthcheck for that.
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["dcfs-server"]
