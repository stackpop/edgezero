#!/usr/bin/env bash
set -euo pipefail

# Opt-in packaging check for a disposable generated app, NOT a production app.
# Prepare portable dependencies and the app's own Cargo.lock before calling this.
# Usage: ./scripts/test_generated_axum_container.sh /path/to/container-probe
# Requires Docker, Python 3.11+, curl, OpenSSL, readelf and network access for image builds.
APP_DIR="$(cd "${1:?pass a prepared disposable generated workspace}" && pwd)"
for tool in docker python3 curl openssl readelf sha256sum; do
  command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done
docker info >/dev/null
NAME="$(python3 - "$APP_DIR/edgezero.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], 'rb') as f:
    print(tomllib.load(f)['app']['name'])
PY
)"
[[ "$NAME" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'Invalid generated app name' >&2; exit 1; }
for file in Dockerfile .dockerignore Cargo.lock .tool-versions; do
  test -f "$APP_DIR/$file" || { echo "Prepare $file first" >&2; exit 1; }
done
WORK="$(mktemp -d -t edgezero-container.XXXXXX)"
ID="edgezero-smoke-$$-$RANDOM"
IMAGE="$ID:app"
PROBE="$ID:context"
NETWORK="$ID"
CONTAINERS=()
cleanup() {
  status=$?
  for container in "${CONTAINERS[@]}"; do
    docker logs "$container" > "$WORK/$container.log" 2>&1 || true
    docker rm -f "$container" >/dev/null 2>&1 || true
  done
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  docker image rm "$IMAGE" "$PROBE" >/dev/null 2>&1 || true
  # Always remove disposable private keys, including on failure.
  rm -f "$WORK/tls/ca.key" "$WORK/tls/server.key"
  if test "$status" -eq 0; then
    rm -rf "$WORK"
  else
    echo "Failed; diagnostics retained at $WORK" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

echo "==> Packaging fixture $NAME; host $(docker info --format '{{.Architecture}}')"
sha256sum "$APP_DIR/Cargo.lock" "$APP_DIR/.tool-versions"
python3 - "$APP_DIR/Cargo.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], 'rb') as f:
    deps = tomllib.load(f)['workspace']['dependencies']
for name, dep in deps.items():
    if name.startswith('edgezero-'):
        print(name, dep)
PY

# Work on a copy so sentinels never alter the supplied workspace.
mkdir "$WORK/context"
cp -a "$APP_DIR/." "$WORK/context/"
CONTEXT="$WORK/context"
for directory in .git .edgezero .wrangler .spin target bin pkg node_modules; do
  mkdir -p "$CONTEXT/$directory"
  printf 'disposable forbidden input\n' > "$CONTEXT/$directory/container-sentinel"
done
mkdir -p "$CONTEXT/.cargo" "$CONTEXT/assets" "$CONTEXT/crates/$NAME-core/src/bin"
printf 'build asset\n' > "$CONTEXT/assets/container-build-asset"
printf 'build source\n' > "$CONTEXT/crates/$NAME-core/src/bin/container-build-source"
printf 'disposable secret\n' > "$CONTEXT/.env.local"
printf 'disposable secret\n' > "$CONTEXT/.dev.vars"
printf 'disposable secret\n' > "$CONTEXT/.cargo/credentials.toml"
printf 'disposable config\n' > "$CONTEXT/$NAME.toml"
# Test Docker's actual context rules, not a substring interpretation of ignore patterns.
cat > "$WORK/context.Dockerfile" <<'DOCKER'
FROM debian:bookworm-slim
COPY . /context
ARG APP_NAME
RUN test ! -e "/context/$APP_NAME.toml" \
    && test -f /context/assets/container-build-asset \
    && test -f "/context/crates/$APP_NAME-core/src/bin/container-build-source" \
    && test -f /context/Cargo.toml && test -f /context/Cargo.lock \
    && test -f /context/edgezero.toml && test -d /context/crates \
    && test -f /context/.tool-versions \
    && test ! -e /context/.env.local && test ! -e /context/.dev.vars \
    && test ! -e /context/.cargo/credentials.toml \
    && ! find /context -name container-sentinel | grep .
DOCKER
docker build --progress=plain --build-arg "APP_NAME=$NAME" -f "$WORK/context.Dockerfile" -t "$PROBE" "$CONTEXT" 2>&1 | tee "$WORK/context.log"
docker build --progress=plain -t "$IMAGE" "$CONTEXT" 2>&1 | tee "$WORK/build.log"
docker image inspect "$IMAGE" --format 'image={{.Id}} architecture={{.Architecture}} user={{.Config.User}}'
docker history --no-trunc "$IMAGE" > "$WORK/history.log"

CONTAINERS+=("$ID-export")
docker create --name "$ID-export" "$IMAGE" >/dev/null
docker export "$ID-export" -o "$WORK/runtime.tar"
docker cp "$ID-export:/etc/ssl/certs/ca-certificates.crt" "$WORK/os-roots.pem"
docker cp "$ID-export:/usr/local/bin/app" "$WORK/app"
readelf -h -l -d "$WORK/app" > "$WORK/elf.log"
grep -E 'Machine:|interpreter|NEEDED' "$WORK/elf.log"
test -s "$WORK/os-roots.pem"
python3 - "$WORK/runtime.tar" <<'PY'
import sys, tarfile
with tarfile.open(sys.argv[1]) as archive:
    paths = {m.name.lstrip('./') for m in archive}
    forbidden = ('build/', 'context/', 'usr/local/cargo/', 'usr/local/rustup/')
    assert not any(p.startswith(forbidden) or '/.git/' in p or p.endswith('container-sentinel') for p in paths)
    for path in ('usr/bin/cargo', 'usr/bin/rustc', 'usr/bin/node', 'usr/bin/npm', 'app/Cargo.toml'):
        assert path not in paths, path
    assert 'usr/local/bin/app' in paths
    header = archive.extractfile('usr/local/bin/app').read(20)
    assert header[:4] == b'\x7fELF', 'not a native ELF executable'
    print('ELF class', header[4], 'machine', int.from_bytes(header[18:20], 'little'))
PY
docker run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges:true \
  --entrypoint sh "$IMAGE" -ec '
    id; test "$(id -u):$(id -g)" = 10001:10001
    grep -q "^CapEff:[[:space:]]*0000000000000000$" /proc/self/status
    grep -q "^NoNewPrivs:[[:space:]]*1$" /proc/self/status
    if touch /app/forbidden 2>/dev/null; then exit 1; fi
  ' 2>&1 | tee "$WORK/identity-libraries.log"
# ldd may return success despite unresolved entries; check its complete output separately.
docker run --rm --read-only --cap-drop=ALL --entrypoint ldd "$IMAGE" /usr/local/bin/app \
  | tee "$WORK/ldd.log"
if grep -q 'not found' "$WORK/ldd.log"; then echo 'Unresolved runtime library' >&2; exit 1; fi

# Explicit tmpfs ownership works; wrong-owner state remains unwritable.
docker run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges:true \
  --tmpfs /app/.edgezero:rw,noexec,nosuid,size=1m,uid=10001,gid=10001,mode=0700 \
  --entrypoint sh "$IMAGE" -ec 'touch /app/.edgezero/check; if touch /app/forbidden 2>/dev/null; then exit 1; fi'
docker run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges:true \
  --tmpfs /app/.edgezero:rw,noexec,nosuid,size=1m,uid=0,gid=0,mode=0700 \
  --entrypoint sh "$IMAGE" -ec '! touch /app/.edgezero/check 2>/dev/null'

# A private bridge supports loopback publication. Docker internal networks may not.
docker network create "$NETWORK" >/dev/null
mkdir "$WORK/tls"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj '/CN=EdgeZero disposable test CA' \
  -keyout "$WORK/tls/ca.key" -out "$WORK/tls/ca.crt" > "$WORK/certificates.log" 2>&1
openssl req -new -newkey rsa:2048 -nodes -subj '/CN=upstream' \
  -keyout "$WORK/tls/server.key" -out "$WORK/tls/server.csr" >> "$WORK/certificates.log" 2>&1
printf 'subjectAltName=DNS:upstream\n' > "$WORK/tls/extensions"
openssl x509 -req -days 1 -in "$WORK/tls/server.csr" -CA "$WORK/tls/ca.crt" \
  -CAkey "$WORK/tls/ca.key" -CAcreateserial -extfile "$WORK/tls/extensions" \
  -out "$WORK/tls/server.crt" >> "$WORK/certificates.log" 2>&1
printf 'edgezero controlled TLS\n' > "$WORK/tls/index.html"
cat "$WORK/os-roots.pem" "$WORK/tls/ca.crt" > "$WORK/trusted.pem"
chmod 644 "$WORK/trusted.pem"
CONTAINERS+=("$ID-upstream")
docker run -d --name "$ID-upstream" --network "$NETWORK" --network-alias upstream \
  --network-alias wrong-host --user "$(id -u):$(id -g)" --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges:true --mount "type=bind,src=$WORK/tls,dst=/tls,readonly" \
  --workdir /tls --entrypoint openssl "$IMAGE" s_server -quiet -accept 8443 \
  -cert /tls/server.crt -key /tls/server.key -WWW >/dev/null

start_app() {
  local case_name="$1" origin="$2"
  shift 2
  local container="$ID-$case_name"
  CONTAINERS+=("$container")
  docker run -d --name "$container" --network "$NETWORK" --read-only --cap-drop=ALL \
    --security-opt=no-new-privileges:true -p 127.0.0.1::8787 \
    -e "API_BASE_URL=$origin" "$@" "$IMAGE" >/dev/null
  PORT="$(docker port "$container" 8787/tcp | awk -F: '{print $NF}')"
  local ready=false
  for _ in $(seq 1 60); do
    if curl --silent --fail --max-time 2 "http://127.0.0.1:$PORT/" > "$WORK/root-body"; then
      ready=true; break
    fi
    sleep 1
  done
  "$ready" || { echo "$case_name never served its published port" >&2; exit 1; }
  test "$(cat "$WORK/root-body")" = "$NAME app"
  docker inspect "$container" --format '{{.HostConfig.ReadonlyRootfs}} {{.HostConfig.CapDrop}} {{.HostConfig.SecurityOpt}}'
}
start_app trusted https://upstream:8443 --mount "type=bind,src=$WORK/trusted.pem,dst=/etc/ssl/certs/ca-certificates.crt,readonly"
# Poll the controlled upstream too; its first request may race server startup.
for _ in $(seq 1 30); do
  curl --silent --fail --max-time 5 "http://127.0.0.1:$PORT/proxy/index.html" > "$WORK/trusted-body" \
    && grep -q 'edgezero controlled TLS' "$WORK/trusted-body" && break
  sleep 1
done
grep -q 'edgezero controlled TLS' "$WORK/trusted-body"
# Only test rejection after proving the same endpoint is reachable and serving TLS.
start_app untrusted https://upstream:8443
status="$(curl --silent --max-time 35 -o "$WORK/untrusted-body" -w '%{http_code}' "http://127.0.0.1:$PORT/proxy/index.html")"
[[ "$status" =~ ^5[0-9][0-9]$ ]]
if grep -q 'edgezero controlled TLS' "$WORK/untrusted-body"; then exit 1; fi
start_app wrong-host https://wrong-host:8443 --mount "type=bind,src=$WORK/trusted.pem,dst=/etc/ssl/certs/ca-certificates.crt,readonly"
status="$(curl --silent --max-time 35 -o "$WORK/wrong-host-body" -w '%{http_code}' "http://127.0.0.1:$PORT/proxy/index.html")"
[[ "$status" =~ ^5[0-9][0-9]$ ]]
if grep -q 'edgezero controlled TLS' "$WORK/wrong-host-body"; then exit 1; fi
echo 'PASS: generated app context, native libraries, UID, read-only root, capabilities, mounts, HTTP and controlled TLS.'
echo 'Packaging only; no readiness, durable restart, SIGTERM or multi-architecture certification.'
