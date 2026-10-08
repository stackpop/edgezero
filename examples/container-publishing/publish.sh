#!/usr/bin/env bash
set -euo pipefail

# Trusted publisher only: verify two same-run bundles, copy their exact OCI bytes,
# and publish a two-platform index. Never builds or executes application images.
# Usage: publish.sh REPOSITORY TAG AMD64_BUNDLE ARM64_BUNDLE OUTPUT REVISION RUN_ID
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tools.env
source "$HERE/tools.env"
REPO="${1:?registry repository}"; TAG="${2:?version tag}"
AMD="${3:?amd64 bundle}"; ARM="${4:?arm64 bundle}"; OUTPUT="${5:?new output directory}"
REVISION="${6:?approved source SHA}"; RUN_ID="${7:?approved workflow run}"
[[ $# -eq 7 && "$TAG" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,127}$ ]] || { echo 'Invalid publication arguments' >&2; exit 2; }
LOCAL=false
if [[ "$REPO" =~ ^127\.0\.0\.1:[0-9]+/[a-z0-9/-]+$ ]]; then
  LOCAL=true
elif [[ ! "$REPO" =~ ^ghcr\.io/[a-z0-9_.-]+/[a-z0-9_.-]+$ ]]; then
  echo 'This recipe supports app-owned GHCR or a disposable loopback registry only' >&2; exit 2
elif [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]]; then
  echo 'GHCR publication requires a version tag, not a reserved alias' >&2; exit 2
fi
SCAN_POLICY="${SCAN_POLICY:-strict}"
[[ "$SCAN_POLICY" == strict || "$SCAN_POLICY" == report-only ]] || { echo 'Invalid scan policy' >&2; exit 2; }
# Report-only evidence may exercise a disposable loopback index, never GHCR.
if ! "$LOCAL" && [[ "$SCAN_POLICY" != strict ]]; then
  echo 'GHCR publication requires strict scan policy' >&2; exit 2
fi
[[ ! -e "$OUTPUT" ]] || { echo 'Output must not exist' >&2; exit 2; }
AMD="$(cd "$AMD" && pwd)"; ARM="$(cd "$ARM" && pwd)"
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
WORK="$(mktemp -d -t axum-publisher.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
VERIFY_ARGS=(--scan-policy "$SCAN_POLICY")
if [[ -n "${SCAN_EXCEPTIONS:-}" ]]; then VERIFY_ARGS+=(--exceptions "$SCAN_EXCEPTIONS"); fi
python3 "$HERE/verify_artifacts.py" verify "$AMD" --revision "$REVISION" --run-id "$RUN_ID" \
  --platform linux/amd64 "${VERIFY_ARGS[@]}" > "$OUTPUT/amd64.json"
python3 "$HERE/verify_artifacts.py" verify "$ARM" --revision "$REVISION" --run-id "$RUN_ID" \
  --platform linux/arm64 "${VERIFY_ARGS[@]}" > "$OUTPUT/arm64.json"
python3 "$HERE/verify_artifacts.py" extract "$AMD/image.tar" "$WORK/amd64" --platform linux/amd64 >/dev/null
python3 "$HERE/verify_artifacts.py" extract "$ARM/image.tar" "$WORK/arm64" --platform linux/arm64 >/dev/null
python3 "$HERE/verify_artifacts.py" index "$AMD" "$ARM" --revision "$REVISION" --run-id "$RUN_ID" \
  "${VERIFY_ARGS[@]}" > "$OUTPUT/index.json"
python3 - "$WORK" "$OUTPUT" <<'PY'
import hashlib, json, pathlib, shutil, sys
work, output = map(pathlib.Path, sys.argv[1:])
merged = work / 'merged'
(merged / 'blobs/sha256').mkdir(parents=True)
for arch in ('amd64', 'arm64'):
    for blob in (work / arch / 'blobs/sha256').iterdir():
        target = merged / 'blobs/sha256' / blob.name
        if target.exists():
            if target.read_bytes() != blob.read_bytes():
                raise ValueError('Conflicting content-addressed blob')
        else:
            shutil.copyfile(blob, target)
raw = (output / 'index.json').read_bytes()
digest = hashlib.sha256(raw).hexdigest()
(merged / 'blobs/sha256' / digest).write_bytes(raw)
(merged / 'oci-layout').write_text('{"imageLayoutVersion":"1.0.0"}')
(merged / 'index.json').write_text(json.dumps({'schemaVersion': 2, 'manifests': [{
    'mediaType': 'application/vnd.oci.image.index.v1+json', 'digest': 'sha256:' + digest, 'size': len(raw),
    'annotations': {'org.opencontainers.image.ref.name': 'release'},
}]}))
(output / 'digest').write_text('sha256:' + digest + '\n')
PY
AUTH=(); TLS=()
if "$LOCAL"; then
  TLS=(--dest-tls-verify=false)
else
  # Docker-compatible auth file must live outside artifacts. Login is workflow-owned.
  AUTH_FILE="${REGISTRY_AUTH_FILE:?provide a protected GHCR auth file}"
  [[ -f "$AUTH_FILE" && "$AUTH_FILE" == /* ]] || { echo 'Expected absolute auth file' >&2; exit 2; }
  AUTH=(--mount "type=bind,src=$AUTH_FILE,dst=/auth.json,readonly")
fi
COPY_AUTH=()
if ! "$LOCAL"; then COPY_AUTH=(--authfile /auth.json); fi
INSPECT_TLS=()
if "$LOCAL"; then INSPECT_TLS=(--tls-verify=false); fi
# Same version may be retried. Only HTTP 404 with a registry not-found code
# permits creation; all other preflight errors abort before any publication.
TAG_AUTH=()
if ! "$LOCAL"; then TAG_AUTH=(--auth-file "$AUTH_FILE"); fi
python3 "$HERE/verify_artifacts.py" tag-check "$REPO" "$TAG" "$(< "$OUTPUT/digest")" \
  "${TAG_AUTH[@]}" > "$OUTPUT/prior-tag.json"
docker run --rm --network host --user "$(id -u):$(id -g)" --cap-drop=ALL \
  --security-opt=no-new-privileges:true --mount "type=bind,src=$WORK,dst=/work,readonly" \
  "${AUTH[@]}" "$SKOPEO_IMAGE" copy --all --preserve-digests "${TLS[@]}" "${COPY_AUTH[@]}" \
  oci:/work/merged:release "docker://$REPO:$TAG"
docker run --rm --network host --user "$(id -u):$(id -g)" --cap-drop=ALL \
  --security-opt=no-new-privileges:true "${AUTH[@]}" "$SKOPEO_IMAGE" inspect --raw \
  "${INSPECT_TLS[@]}" "${COPY_AUTH[@]}" "docker://$REPO:$TAG" > "$OUTPUT/tag.json"
[[ "sha256:$(sha256sum "$OUTPUT/tag.json" | awk '{print $1}')" == "$(< "$OUTPUT/digest")" ]] \
  || { echo 'Version tag moved during publication' >&2; exit 1; }
for subject in "$(< "$OUTPUT/digest")" \
  "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["manifest_digest"])' "$OUTPUT/amd64.json")" \
  "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["manifest_digest"])' "$OUTPUT/arm64.json")"; do
  raw="$OUTPUT/${subject#sha256:}.json"
  docker run --rm --network host --user "$(id -u):$(id -g)" --cap-drop=ALL \
    --security-opt=no-new-privileges:true "${AUTH[@]}" "$SKOPEO_IMAGE" inspect --raw \
    "${INSPECT_TLS[@]}" "${COPY_AUTH[@]}" "docker://$REPO@$subject" > "$raw"
  [[ "sha256:$(sha256sum "$raw" | awk '{print $1}')" == "$subject" ]] \
    || { echo 'Published manifest digest mismatch' >&2; exit 1; }
done
INDEX_DIGEST="$(< "$OUTPUT/digest")"
cmp "$OUTPUT/index.json" "$OUTPUT/${INDEX_DIGEST#sha256:}.json"
echo "Published unchanged runtime children and index: $REPO@$(< "$OUTPUT/digest")"
