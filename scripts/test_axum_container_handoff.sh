#!/usr/bin/env bash
set -euo pipefail

# Native build/test or credential-free import of this recipe's accepted OCI bundle.
# build APP_DIR OUTPUT PLATFORM REVISION RUN_ID
# import BUNDLE_DIR OUTPUT PLATFORM REVISION RUN_ID
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../examples/container-publishing/tools.env
source "$ROOT/examples/container-publishing/tools.env"
VERIFY="$ROOT/examples/container-publishing/verify_artifacts.py"
MODE="${1:?build or import}"; INPUT="${2:?input directory}"; OUTPUT="${3:?new output directory}"
PLATFORM="${4:?linux/amd64 or linux/arm64}"; REVISION="${5:?source SHA}"; RUN_ID="${6:?run identity}"
[[ $# -eq 6 && ( "$MODE" == build || "$MODE" == import ) ]] || { echo 'Invalid handoff arguments' >&2; exit 2; }
# Framework packaging CI may retain scan evidence without approving a release.
SCAN_POLICY="${SCAN_POLICY:-strict}"
[[ "$SCAN_POLICY" == strict || "$SCAN_POLICY" == report-only ]] || { echo 'Invalid scan policy' >&2; exit 2; }
[[ "$PLATFORM" == linux/amd64 || "$PLATFORM" == linux/arm64 ]] || { echo 'Unsupported platform' >&2; exit 2; }
if [[ "$MODE" == build ]]; then
  case "$(docker info --format '{{.Architecture}}')" in
    x86_64|amd64) HOST_PLATFORM=linux/amd64 ;;
    aarch64|arm64) HOST_PLATFORM=linux/arm64 ;;
    *) echo 'Unsupported native Docker host' >&2; exit 2 ;;
  esac
  [[ "$PLATFORM" == "$HOST_PLATFORM" ]] || { echo 'Native build required; emulation is not acceptance' >&2; exit 2; }
fi
VERIFY_ARGS=(--scan-policy "$SCAN_POLICY")
if [[ -n "${SCAN_EXCEPTIONS:-}" ]]; then VERIFY_ARGS+=(--exceptions "$SCAN_EXCEPTIONS"); fi
[[ ! -e "$OUTPUT" ]] || { echo 'Output must not exist' >&2; exit 2; }
INPUT="$(cd "$INPUT" && pwd)"
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
WORK="$(mktemp -d -t edgezero-handoff.XXXXXX)"
ID="edgezero-handoff-$$-$RANDOM"
BUILDER="$ID-builder"
REGISTRY="$ID-registry"
IMAGE=""
cleanup() {
  status=$?
  docker logs "$REGISTRY" > "$OUTPUT/registry.log" 2>&1 || true
  docker rm -f "$REGISTRY" >/dev/null 2>&1 || true
  if [[ "$MODE" == build ]]; then docker buildx rm "$BUILDER" >/dev/null 2>&1 || true; fi
  if [[ -n "$IMAGE" ]]; then docker image rm "$IMAGE" >/dev/null 2>&1 || true; fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

if [[ "$MODE" == build ]]; then
  docker buildx create --name "$BUILDER" --driver docker-container --driver-opt "image=$BUILDKIT_IMAGE" > "$OUTPUT/builder.log"
  docker buildx build --builder "$BUILDER" --platform "$PLATFORM" --provenance=false --sbom=false \
    --label "org.opencontainers.image.revision=$REVISION" \
    --label "org.opencontainers.image.source=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-stackpop/edgezero}" \
    --output "type=oci,dest=$OUTPUT/image.tar,name=app" "$INPUT" 2>&1 | tee "$OUTPUT/build.log"
  cp "$INPUT/fixture-inputs.json" "$OUTPUT/inputs.json"
else
  python3 "$VERIFY" verify "$INPUT" --revision "$REVISION" --run-id "$RUN_ID" --platform "$PLATFORM" \
    "${VERIFY_ARGS[@]}" > "$OUTPUT/verified.json"
  cp "$INPUT/image.tar" "$OUTPUT/image.tar"
fi
python3 "$VERIFY" extract "$OUTPUT/image.tar" "$WORK/oci" --platform "$PLATFORM" > "$OUTPUT/image.json"
DIGEST="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["manifest_digest"])' "$OUTPUT/image.json")"
docker run -d --name "$REGISTRY" -p 127.0.0.1::5000 "$REGISTRY_IMAGE" > "$OUTPUT/registry-id"
PORT="$(docker port "$REGISTRY" 5000/tcp | awk -F: '{print $NF}')"
REG="127.0.0.1:$PORT"
for _ in $(seq 1 30); do
  curl --fail --silent "http://$REG/v2/" >/dev/null && break
  sleep 1
done
curl --fail --silent "http://$REG/v2/" >/dev/null
# Skopeo talks to loopback in the host namespace; only disposable local registry data is exposed.
docker run --rm --network host --user "$(id -u):$(id -g)" --cap-drop=ALL \
  --security-opt=no-new-privileges:true --mount "type=bind,src=$WORK,dst=/work,readonly" \
  "$SKOPEO_IMAGE" copy --all --preserve-digests --dest-tls-verify=false \
  oci:/work/oci "docker://$REG/app:candidate" 2>&1 | tee "$OUTPUT/copy.log"
curl --fail --silent -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
  "http://$REG/v2/app/manifests/$DIGEST" > "$OUTPUT/registry-manifest.json"
[[ "sha256:$(sha256sum "$OUTPUT/registry-manifest.json" | awk '{print $1}')" == "$DIGEST" ]] \
  || { echo 'Registry changed manifest bytes' >&2; exit 1; }
IMAGE="$REG/app@$DIGEST"
docker pull --platform "$PLATFORM" "$IMAGE" > "$OUTPUT/pull.log"
docker image inspect "$IMAGE" > "$OUTPUT/pulled-image.json"
python3 "$VERIFY" pulled "$WORK/oci" "$OUTPUT/pulled-image.json" --platform "$PLATFORM" \
  --reference "$IMAGE" > "$OUTPUT/pulled-verified.json"

if [[ "$MODE" == build ]]; then
  if [[ -n "${APP_SMOKE_SCRIPT:-}" ]]; then
    # Application-owned test script is permitted only in the credential-free build job.
    "$APP_SMOKE_SCRIPT" "$IMAGE" "$PLATFORM"
  else
    "$ROOT/scripts/test_generated_axum_container.sh" "$INPUT" --image "$IMAGE" \
      --platform "$PLATFORM" --evidence-dir "$OUTPUT/smoke"
    APP_NAME="$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["app"]["name"])' "$INPUT/edgezero.toml")"
    "$ROOT/scripts/test_axum_container_deployment.sh" "$IMAGE" "$APP_NAME"
  fi
  mkdir "$OUTPUT/scanner-cache"
  docker run --rm --user "$(id -u):$(id -g)" --mount "type=bind,src=$OUTPUT,dst=/work" \
    --mount "type=bind,src=$WORK/oci,dst=/oci,readonly" \
    "$TRIVY_IMAGE" image --cache-dir /work/scanner-cache --input /oci \
    --scanners vuln --ignorefile /dev/null --format json --output /work/scan.json
  docker run --rm --user "$(id -u):$(id -g)" --mount "type=bind,src=$OUTPUT,dst=/work" \
    "$TRIVY_IMAGE" version --cache-dir /work/scanner-cache --format json > "$OUTPUT/scanner.json"
  docker run --rm --user "$(id -u):$(id -g)" --mount "type=bind,src=$OUTPUT,dst=/work" \
    --mount "type=bind,src=$WORK/oci,dst=/oci,readonly" \
    "$TRIVY_IMAGE" image --cache-dir /work/scanner-cache --input /oci \
    --format cyclonedx --output /work/sbom.json
  python3 - "$OUTPUT" "$DIGEST" "$REVISION" "$RUN_ID" "$BUILDKIT_IMAGE" <<'PY'
import json, pathlib, sys
out = pathlib.Path(sys.argv[1])
inputs = json.loads((out / 'inputs.json').read_text())
(out / 'provenance.json').write_text(json.dumps({
    'subject': sys.argv[2], 'revision': sys.argv[3], 'run_id': sys.argv[4],
    'builder': sys.argv[5], 'inputs': inputs,
    'note': 'Unsigned build receipt. Trusted publisher authentication is separate.',
}, indent=2) + '\n')
PY
  python3 - "$OUTPUT/scanner.json" "$DIGEST" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
metadata = json.loads(path.read_text())
metadata['SubjectManifestDigest'] = sys.argv[2]
path.write_text(json.dumps(metadata, indent=2) + '\n')
PY
  # Seal needs the validated layout, but only the archive/sidecars are transferred.
  ln -s "$WORK/oci" "$OUTPUT/oci"
  python3 "$VERIFY" seal "$OUTPUT" --revision "$REVISION" --run-id "$RUN_ID" --platform "$PLATFORM" > "$OUTPUT/accepted.json"
  rm "$OUTPUT/oci"
  python3 "$VERIFY" verify "$OUTPUT" --revision "$REVISION" --run-id "$RUN_ID" --platform "$PLATFORM" "${VERIFY_ARGS[@]}" > "$OUTPUT/verified.json"
  rm -rf "$OUTPUT/scanner-cache"
fi
# Transfer into a distinct directory and verify/import there in a later job with MODE=import.
printf '%s\n' "$DIGEST" > "$OUTPUT/digest"
echo "PASS: preserved $PLATFORM manifest $DIGEST through OCI registry copy and Docker pull."
