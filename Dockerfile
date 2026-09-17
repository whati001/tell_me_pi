# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin omp-proxy

FROM debian:bookworm-slim
ARG OMP_VERSION=v18.2.3
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git tini \
    && rm -rf /var/lib/apt/lists/*
# git children receive the token in their environment; execute-only binaries make the kernel mark
# them non-dumpable, so the agent (same uid) cannot read /proc/<pid>/environ.
RUN chmod 0711 /usr/bin/git \
    && find /usr/lib/git-core -type f -exec sh -c \
         'if [ "$(head -c 4 "$1" | tr -d "\000")" = "$(printf "\177ELF")" ]; then chmod 0711 "$1"; fi' _ {} \; \
    && ls -l /usr/bin/git
RUN case "${TARGETARCH:-amd64}" in \
        amd64) arch=x64 ;; \
        arm64) arch=arm64 ;; \
        *) echo "unsupported architecture ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /usr/local/bin/omp \
        "https://github.com/can1357/oh-my-pi/releases/download/${OMP_VERSION}/omp-linux-${arch}" \
    && chmod 0755 /usr/local/bin/omp
# HOME lives on the data volume: omp keeps its login (agent.db), caches and settings there.
RUN useradd --uid 10001 --home-dir /data/home --no-create-home --shell /usr/sbin/nologin omp \
    && mkdir -p /data/home/.omp/agent/skills /data/sessions /data/mirrors /etc/omp-proxy \
    && chown -R omp:omp /data
COPY --from=build /src/target/release/omp-proxy /usr/local/bin/omp-proxy
ENV HOME=/data/home \
    OMP_PROXY_CONFIG=/etc/omp-proxy/proxy.toml \
    OMP_PROXY_USER=omp
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --start-period=5s --interval=10s --timeout=5s CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
# Starts as root to read the secrets, then drops to the omp user.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/omp-proxy"]
