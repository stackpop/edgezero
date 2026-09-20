FROM docker.io/library/rust@sha256:6f9e63259f12e1e599296f5ecfed2bae46de4af0ee0525dd8b89c046e236d5c5 AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*

COPY .tool-versions /edgezero-input/.tool-versions
COPY .github/actions/deploy-fastly/versions.json /edgezero-input/fastly-versions.json

RUN set -eux; \
    fastly_version=$(jq -er '.fastly.version' /edgezero-input/fastly-versions.json); \
    fastly_url=$(jq -er '.fastly.linux_amd64.url' /edgezero-input/fastly-versions.json); \
    fastly_sha=$(jq -er '.fastly.linux_amd64.sha256' /edgezero-input/fastly-versions.json); \
    test "$fastly_version" = "15.1.0"; \
    test "$(awk '$1 == "fastly" { print $2 }' /edgezero-input/.tool-versions)" = "$fastly_version"; \
    curl --fail --location --silent --show-error "$fastly_url" --output /tmp/fastly.tar.gz; \
    printf '%s  %s\n' "$fastly_sha" /tmp/fastly.tar.gz | sha256sum --check --strict; \
    tar -xzf /tmp/fastly.tar.gz -C /tmp fastly; \
    install -m 0755 /tmp/fastly /usr/local/bin/fastly; \
    rm -f /tmp/fastly /tmp/fastly.tar.gz

RUN set -eux; \
    asset="sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz"; \
    base="https://github.com/mozilla/sccache/releases/download/v0.10.0"; \
    sccache_sha="1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b"; \
    curl --fail --location --silent --show-error "$base/$asset" --output "/tmp/$asset"; \
    curl --fail --location --silent --show-error "$base/$asset.sha256" --output "/tmp/$asset.sha256"; \
    test "$(wc -c < "/tmp/$asset.sha256")" -eq 64; \
    test "$(cat "/tmp/$asset.sha256")" = "$sccache_sha"; \
    printf '%s  %s\n' "$sccache_sha" "/tmp/$asset" | sha256sum --check --strict; \
    tar -xzf "/tmp/$asset" -C /tmp; \
    install -m 0755 "/tmp/${asset%.tar.gz}/sccache" /usr/local/bin/sccache; \
    rm -rf "/tmp/$asset" "/tmp/$asset.sha256" "/tmp/${asset%.tar.gz}"

RUN rustup target add wasm32-wasip1

WORKDIR /build
COPY .github/tools/edgezero-provenance-validator/Cargo.toml .github/tools/edgezero-provenance-validator/Cargo.lock .github/tools/edgezero-provenance-validator/
COPY .github/tools/edgezero-provenance-validator/src .github/tools/edgezero-provenance-validator/src
COPY .github/tools/edgezero-provenance-validator/tests .github/tools/edgezero-provenance-validator/tests
RUN cargo build --locked --release --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml

FROM docker.io/library/rust@sha256:6f9e63259f12e1e599296f5ecfed2bae46de4af0ee0525dd8b89c046e236d5c5 AS runtime

ARG IMAGE_SOURCE_REVISION
RUN test "${#IMAGE_SOURCE_REVISION}" -eq 40 \
    && case "$IMAGE_SOURCE_REVISION" in *[!0-9a-f]*) exit 1 ;; esac
LABEL org.opencontainers.image.source="https://github.com/stackpop/edgezero" \
      org.opencontainers.image.revision="${IMAGE_SOURCE_REVISION}" \
      io.edgezero.provenance-protocol="1"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
      build-essential ca-certificates curl git jq perl tar \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1001 edgezero \
    && useradd --uid 1001 --gid 1001 --create-home --home-dir /home/edgezero edgezero \
    && mkdir -p /opt/edgezero/runtime-lib /usr/local/share/edgezero /work \
    && cp --dereference /lib/x86_64-linux-gnu/libc.so.6 /opt/edgezero/runtime-lib/libc.so.6 \
    && cp --dereference /lib/x86_64-linux-gnu/libgcc_s.so.1 /opt/edgezero/runtime-lib/libgcc_s.so.1 \
    && cp --dereference /lib/x86_64-linux-gnu/libm.so.6 /opt/edgezero/runtime-lib/libm.so.6 \
    && cp --dereference /lib64/ld-linux-x86-64.so.2 /tmp/ld-linux-x86-64.so.2 \
    && chmod 0755 /tmp/ld-linux-x86-64.so.2 \
    && perl -e 'unlink(q{/lib64}) or die $!; mkdir(q{/lib64}, 0755) or die $!; rename(q{/tmp/ld-linux-x86-64.so.2}, q{/lib64/ld-linux-x86-64.so.2}) or die $!' \
    && rm -f /etc/ld.so.preload \
    && chown 1001:1001 /home/edgezero /work

COPY --from=builder /usr/local/bin/fastly /usr/local/bin/fastly
COPY --from=builder /usr/local/bin/sccache /usr/local/bin/sccache
COPY --from=builder /build/.github/tools/edgezero-provenance-validator/target/release/edgezero-provenance-validator /usr/local/bin/edgezero-provenance-validator
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/components /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/components
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/manifest-rust-std-wasm32-wasip1 /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/manifest-rust-std-wasm32-wasip1
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/multirust-config.toml /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/multirust-config.toml
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/wasm32-wasip1 /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/wasm32-wasip1
COPY .tool-versions /usr/local/share/edgezero/.tool-versions
COPY .github/actions/deploy-fastly/versions.json /usr/local/share/edgezero/fastly-versions.json
COPY .github/docker/build-app-cli/provenance.schema.json /usr/local/share/edgezero/provenance.schema.json
COPY .github/docker/build-app-cli/fixtures/provenance /usr/local/share/edgezero/provenance-fixtures
COPY .github/docker/build-app-cli/fixtures/gnu-smoke.rs /usr/local/share/edgezero/gnu-smoke.rs
COPY .github/docker/build-app-cli/fixtures/wasm-smoke.rs /usr/local/share/edgezero/wasm-smoke.rs
COPY .github/docker/build-app-cli/verify-toolchain.sh /usr/local/bin/verify-toolchain

USER 1001:1001
WORKDIR /work
ENTRYPOINT ["/usr/bin/env"]
