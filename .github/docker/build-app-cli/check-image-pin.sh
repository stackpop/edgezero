#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C

readonly EXPECTED_REPO="ghcr.io/stackpop/edgezero-build-app-cli"
ZERO_SHA256="sha256:$(printf '0%.0s' {1..64})"
readonly ZERO_SHA256
ZERO_SHA1="$(printf '0%.0s' {1..40})"
readonly ZERO_SHA1
readonly MAX_RECORD_BYTES=4096

usage() {
  cat >&2 <<'EOF'
usage:
  check-image-pin.sh <image.json>
  check-image-pin.sh validate <image.json>
  check-image-pin.sh runtime-ref <image.json>
  check-image-pin.sh source-revision <image.json>
  check-image-pin.sh provenance-protocol <image.json>
  check-image-pin.sh validate-pair <image.json> <image-release-evidence.json>
EOF
  exit 2
}

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

require_jq() {
  command -v jq >/dev/null 2>&1 || {
    printf '::error::check-image-pin.sh requires jq\n' >&2
    exit 2
  }
}

require_record_file() {
  local path=$1 label=$2 size
  [[ -f "$path" && ! -L "$path" ]] || die "$label must be a regular, non-symlink file: $path"
  size=$(wc -c <"$path" | tr -d '[:space:]') || die "cannot measure $label: $path"
  [[ "$size" =~ ^[0-9]+$ ]] || die "cannot measure $label: $path"
  ((size > 0 && size <= MAX_RECORD_BYTES)) ||
    die "$label must contain 1..$MAX_RECORD_BYTES bytes: $path"
}

# All fields in both records are scalars. Streaming first preserves repeated
# top-level keys that an ordinary jq object parse would silently overwrite.
require_exact_top_level_keys() {
  local path=$1 label=$2
  shift 2
  local expected_json events keys
  expected_json=$(printf '%s\n' "$@" | jq -Rsc 'split("\n")[:-1]')
  if ! events=$(jq -c --stream . "$path" 2>/dev/null); then
    die "$label is not valid JSON: $path"
  fi
  if ! keys=$(jq -ces --argjson expected "$expected_json" '
      if any(.[]; length == 2 and (.[0] | length) != 1) then
        error("nested value")
      else
        [.[] | select(length == 2) | .[0][0]] as $keys
        | if (($keys | length) == ($keys | unique | length)
              and ($keys | sort) == ($expected | sort))
          then $keys
          else error("wrong or duplicate keys")
          end
      end
    ' <<<"$events" 2>/dev/null); then
    die "$label must contain exactly the required, unique top-level keys: $path"
  fi
  [[ -n "$keys" ]] || die "$label has no fields: $path"
}

require_single_object() {
  local path=$1 label=$2
  jq -ces 'if length == 1 and (.[0] | type) == "object" then .[0] else error("not one object") end' \
    "$path" 2>/dev/null || die "$label must be exactly one JSON object: $path"
}

is_nonzero_digest() {
  is_digest "$1" && [[ "$1" != "$ZERO_SHA256" ]]
}

is_digest() {
  [[ "$1" =~ ^sha256:[0-9a-f]{64}$ ]]
}

is_nonzero_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != "$ZERO_SHA1" ]]
}

is_release_tag() {
  [[ "$1" =~ ^build-container-v[1-9][0-9]*$ ]]
}

is_github_login() {
  [[ "$1" =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$ && "$1" != *--* ]]
}

is_positive_decimal_at_most() {
  local value=$1 maximum=$2
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

validate_image() {
  local path=$1 json
  require_record_file "$path" "image record"
  require_exact_top_level_keys "$path" "image record" \
    repository tag digest image-source-revision provenance-protocol
  json=$(require_single_object "$path" "image record")

  jq -e '
      (.repository | type) == "string"
      and (.tag | type) == "string"
      and (.digest | type) == "string"
      and (."image-source-revision" | type) == "string"
      and (."provenance-protocol" | type) == "number"
      and ."provenance-protocol" == 1
    ' <<<"$json" >/dev/null || die "image record fields have invalid JSON types or protocol"

  IMAGE_REPOSITORY=$(jq -r '.repository' <<<"$json")
  IMAGE_TAG=$(jq -r '.tag' <<<"$json")
  IMAGE_DIGEST=$(jq -r '.digest' <<<"$json")
  IMAGE_SOURCE_REVISION=$(jq -r '."image-source-revision"' <<<"$json")
  IMAGE_PROVENANCE_PROTOCOL=$(jq -r '."provenance-protocol"' <<<"$json")

  [[ "$IMAGE_REPOSITORY" == "$EXPECTED_REPO" ]] ||
    die "image repository must be $EXPECTED_REPO"
  is_release_tag "$IMAGE_TAG" || die "image tag is not canonical: $IMAGE_TAG"
  is_nonzero_digest "$IMAGE_DIGEST" || die "image digest is not a nonzero sha256 digest"
  is_nonzero_sha "$IMAGE_SOURCE_REVISION" || die "image source revision is not a nonzero SHA"
  [[ "$IMAGE_PROVENANCE_PROTOCOL" == 1 ]] || die "image provenance protocol must equal 1"
}

validate_review_time() {
  local value=$1 epoch round_trip
  [[ "$value" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    die "reviewed-at must use exact YYYY-MM-DDTHH:MM:SSZ UTC form"
  epoch=$(jq -nr --arg value "$value" '$value | fromdateiso8601' 2>/dev/null) ||
    die "reviewed-at is not a valid UTC instant"
  round_trip=$(jq -nr --argjson epoch "$epoch" '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")') ||
    die "reviewed-at cannot be normalized"
  [[ "$round_trip" == "$value" ]] || die "reviewed-at is not a valid calendar instant"
}

validate_evidence() {
  local path=$1 json canonical_json
  require_record_file "$path" "release evidence"
  require_exact_top_level_keys "$path" "release evidence" \
    approval-challenge approver-login image-digest release-tag reviewed-at run-attempt run-id \
    schema-version screenshot-sha256 source-revision
  json=$(require_single_object "$path" "release evidence")

  jq -e '
      (."approval-challenge" | type) == "string"
      and (."approver-login" | type) == "string"
      and (."image-digest" | type) == "string"
      and (."release-tag" | type) == "string"
      and (."reviewed-at" | type) == "string"
      and (."run-attempt" | type) == "string"
      and (."run-id" | type) == "string"
      and (."schema-version" | type) == "number"
      and ."schema-version" == 1
      and (."screenshot-sha256" | type) == "string"
      and (."source-revision" | type) == "string"
    ' <<<"$json" >/dev/null || die "release evidence fields have invalid JSON types or schema"

  EVIDENCE_APPROVAL_CHALLENGE=$(jq -r '."approval-challenge"' <<<"$json")
  EVIDENCE_APPROVER_LOGIN=$(jq -r '."approver-login"' <<<"$json")
  EVIDENCE_IMAGE_DIGEST=$(jq -r '."image-digest"' <<<"$json")
  EVIDENCE_RELEASE_TAG=$(jq -r '."release-tag"' <<<"$json")
  EVIDENCE_REVIEWED_AT=$(jq -r '."reviewed-at"' <<<"$json")
  EVIDENCE_RUN_ATTEMPT=$(jq -r '."run-attempt"' <<<"$json")
  EVIDENCE_RUN_ID=$(jq -r '."run-id"' <<<"$json")
  EVIDENCE_SCREENSHOT_SHA256=$(jq -r '."screenshot-sha256"' <<<"$json")
  EVIDENCE_SOURCE_REVISION=$(jq -r '."source-revision"' <<<"$json")

  [[ "$EVIDENCE_APPROVAL_CHALLENGE" =~ ^[0-9a-f]{64}$ ]] ||
    die "approval challenge is not a 64-lowercase-hex value"
  is_github_login "$EVIDENCE_APPROVER_LOGIN" || die "approver login is not canonical"
  is_nonzero_digest "$EVIDENCE_IMAGE_DIGEST" || die "evidence image digest is invalid"
  is_release_tag "$EVIDENCE_RELEASE_TAG" || die "evidence release tag is invalid"
  validate_review_time "$EVIDENCE_REVIEWED_AT"
  is_positive_decimal_at_most "$EVIDENCE_RUN_ATTEMPT" 4294967295 ||
    die "run attempt is not a canonical positive u32"
  is_positive_decimal_at_most "$EVIDENCE_RUN_ID" 18446744073709551615 ||
    die "run id is not a canonical positive u64"
  is_digest "$EVIDENCE_SCREENSHOT_SHA256" || die "screenshot digest is invalid"
  is_nonzero_sha "$EVIDENCE_SOURCE_REVISION" || die "evidence source revision is invalid"

  canonical_json=$(jq -cnS \
    --arg challenge "$EVIDENCE_APPROVAL_CHALLENGE" \
    --arg login "$EVIDENCE_APPROVER_LOGIN" \
    --arg digest "$EVIDENCE_IMAGE_DIGEST" \
    --arg tag "$EVIDENCE_RELEASE_TAG" \
    --arg reviewed "$EVIDENCE_REVIEWED_AT" \
    --arg attempt "$EVIDENCE_RUN_ATTEMPT" \
    --arg run_id "$EVIDENCE_RUN_ID" \
    --arg screenshot "$EVIDENCE_SCREENSHOT_SHA256" \
    --arg source "$EVIDENCE_SOURCE_REVISION" \
    '{"approval-challenge":$challenge,"approver-login":$login,"image-digest":$digest,
      "release-tag":$tag,"reviewed-at":$reviewed,"run-attempt":$attempt,"run-id":$run_id,
      "schema-version":1,"screenshot-sha256":$screenshot,"source-revision":$source}')
  cmp -s <(printf '%s' "$canonical_json") "$path" ||
    die "release evidence bytes are not exact RFC 8785 JCS"
}

validate_pair() {
  validate_image "$1"
  validate_evidence "$2"
  [[ "$EVIDENCE_IMAGE_DIGEST" == "$IMAGE_DIGEST" ]] || die "image digest differs across records"
  [[ "$EVIDENCE_RELEASE_TAG" == "$IMAGE_TAG" ]] || die "release tag differs across records"
  [[ "$EVIDENCE_SOURCE_REVISION" == "$IMAGE_SOURCE_REVISION" ]] ||
    die "source revision differs across records"
}

main() {
  require_jq
  local mode path
  if (($# == 1)); then
    mode=validate
    path=$1
  elif (($# == 2)); then
    mode=$1
    path=$2
  elif (($# == 3)) && [[ "$1" == validate-pair ]]; then
    validate_pair "$2" "$3"
    printf 'image release record pair is valid\n'
    return
  else
    usage
  fi

  case "$mode" in
    validate)
      validate_image "$path"
      printf 'image pin is valid\n'
      ;;
    runtime-ref)
      validate_image "$path"
      printf '%s@%s\n' "$IMAGE_REPOSITORY" "$IMAGE_DIGEST"
      ;;
    source-revision)
      validate_image "$path"
      printf '%s\n' "$IMAGE_SOURCE_REVISION"
      ;;
    provenance-protocol)
      validate_image "$path"
      printf '%s\n' "$IMAGE_PROVENANCE_PROTOCOL"
      ;;
    *) usage ;;
  esac
}

main "$@"
