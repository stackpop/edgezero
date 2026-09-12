#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly CHECK="$DIR/check-image-pin.sh"

usage() {
  cat >&2 <<'EOF'
usage: write-image-release-record.sh \
  --image-path <image.json> \
  --evidence-path <image-release-evidence.json> \
  --repository <repository> \
  --release-tag <tag> \
  --image-digest <sha256:digest> \
  --source-revision <sha> \
  --provenance-protocol <integer> \
  --approval-challenge <64-lowercase-hex> \
  --approver-login <login> \
  --reviewed-at <YYYY-MM-DDTHH:MM:SSZ> \
  --run-attempt <canonical-positive-u32> \
  --run-id <canonical-positive-u64> \
  --screenshot-sha256 <sha256:digest>
EOF
  exit 2
}

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

image_path=
evidence_path=
repository=
release_tag=
image_digest=
source_revision=
provenance_protocol=
approval_challenge=
approver_login=
reviewed_at=
run_attempt=
run_id=
screenshot_sha256=
seen_flags=" "

while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$flag" in
    --image-path | --evidence-path | --repository | --release-tag | --image-digest | \
      --source-revision | --provenance-protocol | --approval-challenge | --approver-login | \
      --reviewed-at | --run-attempt | --run-id | --screenshot-sha256) ;;
    *) usage ;;
  esac
  [[ "$seen_flags" != *" $flag "* ]] || die "duplicate argument: $flag"
  seen_flags+="$flag "
  case "$flag" in
    --image-path) image_path=$value ;;
    --evidence-path) evidence_path=$value ;;
    --repository) repository=$value ;;
    --release-tag) release_tag=$value ;;
    --image-digest) image_digest=$value ;;
    --source-revision) source_revision=$value ;;
    --provenance-protocol) provenance_protocol=$value ;;
    --approval-challenge) approval_challenge=$value ;;
    --approver-login) approver_login=$value ;;
    --reviewed-at) reviewed_at=$value ;;
    --run-attempt) run_attempt=$value ;;
    --run-id) run_id=$value ;;
    --screenshot-sha256) screenshot_sha256=$value ;;
  esac
done

for required in \
  --image-path --evidence-path --repository --release-tag --image-digest --source-revision \
  --provenance-protocol --approval-challenge --approver-login --reviewed-at --run-attempt \
  --run-id --screenshot-sha256; do
  [[ "$seen_flags" == *" $required "* ]] || die "missing required argument: $required"
done

[[ -n "$image_path" && -n "$evidence_path" ]] || die "record paths must not be empty"
[[ "$image_path" != "$evidence_path" ]] || die "image and evidence paths must differ"
command -v jq >/dev/null 2>&1 || die "write-image-release-record.sh requires jq"
command -v git >/dev/null 2>&1 || die "write-image-release-record.sh requires git"

[[ "$image_path" == /* && "$evidence_path" == /* ]] ||
  die "record paths must be absolute"

image_parent_arg=$(dirname -- "$image_path")
evidence_parent_arg=$(dirname -- "$evidence_path")
[[ ! -L "$image_parent_arg" && ! -L "$evidence_parent_arg" ]] ||
  die "record output parent must not be a symlink"
image_parent=$(cd -- "$image_parent_arg" 2>/dev/null && pwd -P) ||
  die "image output parent must already exist"
evidence_parent=$(cd -- "$evidence_parent_arg" 2>/dev/null && pwd -P) ||
  die "evidence output parent must already exist"
[[ -d "$image_parent" ]] ||
  die "image output parent must be a non-symlink directory"
[[ -d "$evidence_parent" ]] ||
  die "evidence output parent must be a non-symlink directory"
[[ "$image_parent" == "$evidence_parent" ]] || die "image and evidence outputs must share one parent"
[[ "$image_path" == "$image_parent/$(basename -- "$image_path")" &&
  "$evidence_path" == "$evidence_parent/$(basename -- "$evidence_path")" ]] ||
  die "record paths must be direct children of their canonical parent"

if stat -f '%Lp' "$image_parent" >/dev/null 2>&1; then
  parent_mode=$(stat -f '%Lp' "$image_parent")
else
  parent_mode=$(stat -c '%a' "$image_parent")
fi
[[ "$parent_mode" == 700 ]] || die "record output parent must have mode 0700"

inside_work_tree=$(env -i HOME="${HOME:-/}" PATH="$PATH" LC_ALL=C \
  git -C "$image_parent" rev-parse --is-inside-work-tree 2>/dev/null || true)
inside_git_dir=$(env -i HOME="${HOME:-/}" PATH="$PATH" LC_ALL=C \
  git -C "$image_parent" rev-parse --is-inside-git-dir 2>/dev/null || true)
if [[ "$inside_work_tree" == true || "$inside_git_dir" == true ]]; then
  die "record output parent must be outside every Git repository"
fi

[[ ! -e "$image_path" && ! -L "$image_path" ]] || die "image output already exists"
[[ ! -e "$evidence_path" && ! -L "$evidence_path" ]] || die "evidence output already exists"
[[ "$provenance_protocol" == 1 ]] || die "provenance protocol must be the exact integer spelling 1"

image_json=$(jq -cnS \
  --arg repository "$repository" \
  --arg tag "$release_tag" \
  --arg digest "$image_digest" \
  --arg source "$source_revision" \
  --arg protocol "$provenance_protocol" \
  '{digest:$digest,"image-source-revision":$source,"provenance-protocol":($protocol | tonumber),
    repository:$repository,tag:$tag}' 2>/dev/null) || die "provenance protocol is not numeric"
evidence_json=$(jq -cnS \
  --arg challenge "$approval_challenge" \
  --arg login "$approver_login" \
  --arg digest "$image_digest" \
  --arg tag "$release_tag" \
  --arg reviewed "$reviewed_at" \
  --arg attempt "$run_attempt" \
  --arg run_id "$run_id" \
  --arg screenshot "$screenshot_sha256" \
  --arg source "$source_revision" \
  '{"approval-challenge":$challenge,"approver-login":$login,"image-digest":$digest,
    "release-tag":$tag,"reviewed-at":$reviewed,"run-attempt":$attempt,"run-id":$run_id,
    "schema-version":1,"screenshot-sha256":$screenshot,"source-revision":$source}')

image_tmp=
evidence_tmp=
image_published=false
evidence_published=false
cleanup() {
  status=$?
  trap - EXIT
  if [[ "$evidence_published" == true && "$image_published" != true &&
    -n "$evidence_tmp" && -e "$evidence_path" && "$evidence_path" -ef "$evidence_tmp" ]]; then
    rm -f -- "$evidence_path"
  fi
  if [[ "$image_published" == true && "$evidence_published" != true &&
    -n "$image_tmp" && -e "$image_path" && "$image_path" -ef "$image_tmp" ]]; then
    rm -f -- "$image_path"
  fi
  [[ -z "$image_tmp" ]] || rm -f -- "$image_tmp"
  [[ -z "$evidence_tmp" ]] || rm -f -- "$evidence_tmp"
  exit "$status"
}
trap cleanup EXIT

image_tmp=$(mktemp "$image_parent/.edgezero-image-record.XXXXXX") || die "cannot create image record"
evidence_tmp=$(mktemp "$evidence_parent/.edgezero-release-evidence.XXXXXX") ||
  die "cannot create release evidence"
printf '%s' "$image_json" >"$image_tmp"
printf '%s' "$evidence_json" >"$evidence_tmp"
chmod 0644 "$image_tmp" "$evidence_tmp"

bash "$CHECK" validate-pair "$image_tmp" "$evidence_tmp" >/dev/null

ln "$image_tmp" "$image_path" || die "image output appeared before publication"
image_published=true
ln "$evidence_tmp" "$evidence_path" || die "evidence output appeared before publication"
evidence_published=true
