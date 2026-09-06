#!/usr/bin/env bash
set -euo pipefail

# Validate the two reviewed syntax additions before adapting them for 1.7.12.
# Diagnostics are never filtered; only approved source lines are rewritten.
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
[[ "$(actionlint -version | sed -n '1p')" == 1.7.12 ]] || {
  echo 'error: run-actionlint requires actionlint 1.7.12' >&2; exit 1;
}
[[ "$(yq --version)" == 'yq (https://github.com/mikefarah/yq/) version v4.53.3' ]] || {
  echo 'error: run-actionlint requires mikefarah yq 4.53.3' >&2; exit 1;
}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
files=()
if [[ "$#" -gt 0 ]]; then
  files=("$@")
else
  find "$root/.github/workflows" -maxdepth 1 \( -type f -o -type l \) \( -name '*.yml' -o -name '*.yaml' \) -print0 >"$tmp/files"
  while IFS= read -r -d '' file; do files+=("$file"); done <"$tmp/files"
fi
index=0
status=0
for input in ${files[@]+"${files[@]}"}; do
  [[ -f "$input" && ! -L "$input" ]] || { echo "error: missing or symlink workflow $input" >&2; exit 1; }
  file=$(cd -- "$(dirname -- "$input")" && pwd -P)/$(basename -- "$input")
  relative=${file#"$root/"}
  index=$((index + 1))
  source_copy="$tmp/$index.yml"
  yq -o=json -I=0 '{"document": ., "aliases": [... | select(kind == "alias") | path], "duplicates": [.. | select(kind == "map") | to_entries | group_by(.key) | .[] | select(length > 1)], "nodes": [.. | {"path": path, "line": line, "kind": kind, "tag": tag, "style": style, "value": .}]}' "$file" >"$tmp/parsed"
  if ! jq -se --arg file "$relative" '
    if length != 1 then error("expected one YAML document") else .[0] end
    | if (.aliases | length) != 0 or (.duplicates | length) != 0 then error("aliases and duplicate keys are unsupported") else . end
    | . as $tree
    | [ .nodes[] | select(.path[-1] == "queue") ] as $queues
    | if ($queues | length) > 0 then
        if ($file != ".github/workflows/publish-build-container.yml" and $file != ".github/workflows/rotate-build-container-gate.yml")
          or ($queues | length) != 1 or $queues[0].path != ["concurrency", "queue"]
          or $queues[0].tag != "!!str" or $queues[0].value != "max"
          or .document.concurrency.group != "edgezero-build-container-publication"
          or .document.concurrency["cancel-in-progress"] != false
        then error("unapproved concurrency.queue") else . end
      else . end
    | {workflow_repository: "EDGEZERO_WORKFLOW_REPOSITORY", workflow_file_path: "EDGEZERO_WORKFLOW_FILE_PATH", workflow_ref: "EDGEZERO_WORKFLOW_REF", workflow_sha: "EDGEZERO_WORKFLOW_SHA"} as $bindings
    | [ .nodes[] | select(.kind == "scalar" and .tag == "!!str")
        | select(.value | test("job\\s*(\\.|\\[).*workflow_"))
        | . as $node
        | if $file != ".github/workflows/build-app-cli.yml" then error("workflow identity outside producer") else . end
        | [ $bindings | keys[] | select($node.value == ("${{ job." + . + " }}")) ] as $properties
        | if ($properties | length) != 1 then error("unapproved workflow identity expression") else . end
        | $properties[0] as $property
        | if .path == ["jobs", "build", "steps", 0, "env", $bindings[$property]] then .
          elif $property == "workflow_sha" and .path == ["jobs", "build", "steps", 1, "with", "ref"]
            and $tree.document.jobs.build.steps[1].uses == "actions/checkout@v7.0.1"
            and $tree.document.jobs.build.steps[1].with.repository == "stackpop/edgezero"
          then . else error("unapproved workflow identity location") end
        | {line, path, key: .path[-1], value: "edgezero-validated-identity"}
      ] as $identities
    | ($queues | map({line, path, key: "queue", value: null})) + $identities
    | if any(.[]; . as $rewrite | any($tree.nodes[];
        .style == "flow" and .path == $rewrite.path[0:(.path | length)]))
      then error("rewritten scalars cannot have flow-style ancestors") else . end
    | if (group_by(.line) | any(length > 1)) then error("rewrites must occupy separate source lines") else . end
  ' "$tmp/parsed" >"$tmp/rewrite-data"; then
    echo "error: unsupported workflow compatibility syntax in $file" >&2
    exit 1
  fi
  jq -r '.[] | [.line, .key, (.value // "")] | @tsv' "$tmp/rewrite-data" >"$tmp/rewrites"
  awk -F '\t' '
    FILENAME == ARGV[1] { keys[$1]=$2; values[$1]=$3; next }
    FNR in keys {
      key=keys[FNR]
      if ($0 !~ ("^[ ]*" key ":[ ]*[^ ]")) { print "error: rewrite requires a standalone scalar line" > "/dev/stderr"; exit 1 }
      if (key == "queue") { print ""; next }
      match($0, /[^ ]/)
      print substr($0, 1, RSTART-1) key ": \"" values[FNR] "\""
      next
    }
    { print }
  ' "$tmp/rewrites" "$file" >"$source_copy"
  # The real source filename preserves actionlint project discovery and local
  # reusable-workflow checks while input bytes come from the reviewed copy.
  if actionlint -oneline -shellcheck='shellcheck -S warning' -stdin-filename "$file" - <"$source_copy" >"$tmp/diagnostics" 2>&1; then
    :
  else
    status=1
  fi
  # Literal prefix replacement preserves every diagnostic, including unknown ones.
  while IFS= read -r line; do
    case "$line" in
      "$source_copy":*) printf '%s%s\n' "$file" "${line#"$source_copy"}" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done <"$tmp/diagnostics"
done
[[ "$index" -gt 0 ]] || { echo 'error: no workflows found' >&2; exit 1; }
exit "$status"
