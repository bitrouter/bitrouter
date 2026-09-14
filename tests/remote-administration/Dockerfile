# Container acceptance image for docs/REMOTE_ADMINISTRATION_SPEC.md.
#
# The image deliberately contains the server binary and only generic client
# tooling. Each run mounts server state into the server container and gives the
# client an empty home, so no host configuration or provider credential crosses
# the network-boundary test.
FROM rust:1.93-bookworm AS build

WORKDIR /src
COPY . .
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_INCREMENTAL=false \
    CARGO_PROFILE_DEV_DEBUG=0
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --locked --package bitrouter \
    && install -D -m 0755 /src/target/debug/bro /out/bro \
    && /bin/bash -o pipefail -c 'cargo test --locked --package bitrouter --lib --no-run --message-format=json | python3 tests/remote-administration/select-test-binary.py /out/bitrouter-lib-tests'

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        jq \
        openssl \
        python3 \
        socat \
    && rm -rf /var/lib/apt/lists/*
RUN install -d -m 0755 /tls

COPY --from=build /out/bro /usr/local/bin/bro
COPY --from=build /out/bitrouter-lib-tests /usr/local/bin/bitrouter-lib-tests
COPY tests/remote-administration/mock-openai.py /usr/local/libexec/mock-openai.py
COPY tests/remote-administration/server-entrypoint.sh /usr/local/bin/remote-admin-server
COPY tests/remote-administration/tls-proxy-entrypoint.sh /usr/local/bin/remote-admin-tls-proxy
COPY tests/remote-administration/client-entrypoint.sh /usr/local/bin/remote-admin-client
COPY tests/remote-administration/pty-entrypoint.sh /usr/local/bin/remote-admin-pty
COPY tests/remote-administration/client-checks.sh /usr/local/bin/remote-admin-checks
COPY tests/remote-administration/disconnect-submit.py /usr/local/libexec/disconnect-submit.py
COPY tests/remote-administration/mcp-check.py /usr/local/libexec/mcp-check.py
COPY tests/remote-administration/target-isolation.py /usr/local/libexec/target-isolation.py
COPY tests/remote-administration/pty-check.py /usr/local/libexec/pty-check.py

RUN chmod 0755 \
        /usr/local/bin/remote-admin-server \
        /usr/local/bin/remote-admin-tls-proxy \
        /usr/local/bin/remote-admin-client \
        /usr/local/bin/remote-admin-pty \
        /usr/local/bin/remote-admin-checks \
        /usr/local/libexec/mock-openai.py \
        /usr/local/libexec/disconnect-submit.py

CMD ["bash"]
