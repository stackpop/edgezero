#!/usr/bin/env bash
set -euo pipefail

# Installs stateful fake Fastly CLI/API surfaces for the hosted lifecycle smoke.
# The state models an active source version, typed resource links, exact selected
# Config/KV/Secret resources, provider-visible package identity, and publication.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

write_fake_fastly() {
  local path="$1" version="$2"
  cat >"$path" <<SHIM
#!/usr/bin/env bash
set -euo pipefail
printf 'fastly %s\n' "\$*" >>"\$FAKE_CALL_LOG"

arg_value() {
  local prefix="\$1" arg
  shift
  for arg in "\$@"; do
    case "\$arg" in "\$prefix"*) printf '%s' "\${arg#"\$prefix"}"; return 0;; esac
  done
  return 1
}

links_file() { printf '%s/version-%s.tsv' "\$FAKE_LINK_DIR" "\$1"; }
links_json() {
  local file
  file=\$(links_file "\$1")
  if [[ ! -s "\$file" ]]; then printf '[]\n'; return; fi
  jq -Rn '[inputs | split("\\t") | {id: .[0], name: .[1], resource_id: .[2], resource_type: .[3]}]' <"\$file"
}

case "\${1:-} \${2:-}" in
  'version ' | '--version ') echo 'Fastly CLI version v$version (fake)' ;;
  'config-store list')
    cat <<'JSON'
[{"id":"ENVSEL1","name":"edgezero_runtime_env"},{"id":"CONFIGPROD","name":"config-prod"},{"id":"CONFIGSTAGE","name":"config-stage"}]
JSON
    ;;
  'resource-link list')
    [[ "\$#" -eq 5 && "\$3" == --service-id=dummyservice &&
      ("\$4" == --version=40 || "\$4" == --version=42) && "\$5" == --json ]] || exit 91
    target=\$(arg_value --version= "\$@") || exit 91
    [[ -f "\$(links_file "\$target")" ]] || exit 91
    links_json "\$target"
    ;;
  'compute hash-files')
    [[ "\$#" -eq 6 && "\$3" == --package=* && "\$4" == --skip-build &&
      "\$5" == --non-interactive && "\$6" == --quiet ]] || exit 92
    package=\${3#--package=}
    [[ -f "\$package" && ! -L "\$package" ]] || exit 92
    printf '%0128d\n' 0
    ;;
  'compute update')
    [[ "\$#" -eq 7 && "\$3" == --service-id=dummyservice && "\$4" == --autoclone &&
      "\$5" == --version=active && "\$6" == --package=* && "\$7" == --non-interactive ]] || exit 92
    package=\${6#--package=}
    [[ -f "\$package" && ! -L "\$package" ]] || exit 92
    grep -qx 40 "\$FAKE_VERSION_FILE" || exit 92
    grep -qx 42 "\$FAKE_VERSION_FILE" || printf '42\n' >>"\$FAKE_VERSION_FILE"
    cp "\$(links_file 40)" "\$(links_file 42)"
    digest=\$(sha256sum "\$package" | awk '{print \$1}')
    printf '%s\n' "\$digest" >"\$FAKE_PACKAGE_DIGEST_FILE"
    printf 'PACKAGE-SHA256 %s\n' "\$digest" >>"\$FAKE_CALL_LOG"
    if [[ -n "\${FAKE_EXPECTED_PACKAGE_DIGEST:-}" && "\$digest" != "\$FAKE_EXPECTED_PACKAGE_DIGEST" ]]; then
      echo 'fake fastly: immutable package digest changed' >&2
      exit 93
    fi
    echo 'SUCCESS: Updated package (service dummyservice, version 42)'
    ;;
  'service-version update')
    [[ "\$#" -eq 6 && "\$3" == --service-id=dummyservice && "\$4" == --version=42 &&
      "\$5" == --comment ]] || exit 94
    case "\$6" in
      'production smoke' | 'staged smoke' | 'store-free managed smoke') ;;
      *) exit 94;;
    esac
    ;;
  'service-version stage')
    [[ "\$*" == 'service-version stage --service-id=dummyservice --version=42' ]] || exit 94
    grep -qx 42 "\$FAKE_VERSION_FILE" || exit 94
    [[ -s "\$FAKE_PACKAGE_DIGEST_FILE" && -f "\$FAKE_LINK_DIR/version-42.tsv" ]] || exit 94
    printf '42\n' >"\$FAKE_STAGED_VERSION_FILE"
    ;;
  'resource-link delete')
    [[ "\$#" -eq 5 && "\$3" == --service-id=dummyservice && "\$4" == --version=42 ]] || exit 91
    if [[ -n "\${FAKE_FAIL_AFTER_VERSION:-}" ]]; then
      echo 'simulated post-upload resource-link failure' >&2
      exit 77
    fi
    target=\$(arg_value --version= "\$@") || exit 91
    id=\$(arg_value --id= "\$@") || exit 91
    file=\$(links_file "\$target")
    grep -q "^\$id"$'\\t' "\$file" || exit 91
    awk -F '\\t' -v id="\$id" '\$1 != id' "\$file" >"\$file.tmp"
    mv "\$file.tmp" "\$file"
    ;;
  'resource-link create')
    [[ "\$#" -eq 6 && "\$3" == --service-id=dummyservice && "\$4" == --version=42 ]] || exit 91
    target=\$(arg_value --version= "\$@") || exit 91
    resource=\$(arg_value --resource-id= "\$@") || exit 91
    alias=\$(arg_value --name= "\$@") || exit 91
    case "\$resource/\$alias" in
      CONFIGSTAGE/app_config) type=config_store ;;
      KVSTAGE/cache) type=kv_store ;;
      SECRETSTAGE/credentials) type=secret_store ;;
      *) exit 91;;
    esac
    file=\$(links_file "\$target")
    ! awk -F '\t' -v alias="\$alias" -v type="\$type" '\$2 == alias && \$4 == type { found = 1 } END { exit !found }' "\$file" || exit 91
    printf 'LINK_%s\t%s\t%s\t%s\n' "\$alias" "\$alias" "\$resource" "\$type" >>"\$file"
    ;;
  'config-store-entry describe')
    echo 'fake fastly: unexpected Config Store describe' >&2
    exit 96
    ;;
  'config-store-entry update')
    key=\$(arg_value --key= "\$@") || exit 91
    cat >"\$FAKE_CONFIG_PUSH_DIR/\$key"
    ;;
  'config-store-entry list') echo '[]' ;;
  *) echo "fake fastly: unhandled command: \$*" >&2; exit 90 ;;
esac
SHIM
  chmod +x "$path"
}

write_fake_curl() {
  local path="$1"
  cat >"$path" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail

out=''
url=''
previous=''
for arg in "$@"; do
  [[ "$previous" == --output ]] && out="$arg"
  case "$arg" in file://*) url="$arg";; esac
  previous="$arg"
done
if [[ -n "$out" ]]; then cp "${url#file://}" "$out"; exit 0; fi

active_version() {
  local active
  active=$(cat "$FAKE_ACTIVE_VERSION_FILE" 2>/dev/null || true)
  printf '%s' "${active:-40}"
}

version_list() {
  local active staged target=false
  active=$(active_version)
  staged=$(cat "$FAKE_STAGED_VERSION_FILE" 2>/dev/null || true)
  [[ -f "$FAKE_VERSION_FILE" ]] && grep -qx 42 "$FAKE_VERSION_FILE" && target=true
  printf '[{"number":40,"active":%s,"locked":true,"staging":false,"deployed":true,"environments":[]}' "$([[ "$active" == 40 ]] && echo true || echo false)"
  if [[ "$target" == true ]]; then
    if [[ "$active" == 42 ]]; then
      printf ',{"number":42,"active":true,"locked":true,"staging":false,"deployed":true,"environments":[]}'
    elif [[ "$staged" == 42 ]]; then
      printf ',{"number":42,"active":false,"locked":true,"staging":false,"deployed":true,"environments":[{"active_version":42,"name":"staging","service_id":"dummyservice"}]}'
    else
      printf ',{"number":42,"active":false,"locked":false,"staging":false,"deployed":false,"environments":[]}'
    fi
  fi
  if [[ "$active" != 40 && "$active" != 42 ]]; then
    printf ',{"number":%s,"active":true,"locked":true,"staging":false,"deployed":true,"environments":[]}' "$active"
  fi
  printf ']'
}

if [[ "$*" == *--config* ]]; then
  config=$(cat)
  url=$(printf '%s\n' "$config" | sed -nE 's/^url = "(.*)"$/\1/p')
  request=$(printf '%s\n' "$config" | sed -nE 's/^request = "(.*)"$/\1/p')
  request=${request:-GET}
  printf '%s %s\n' "$request" "$url" >>"$FAKE_CALL_LOG"
  if [[ "$request" == PUT ]]; then
    case "$url" in
      */service/dummyservice/version/42/activate)
        [[ -s "$FAKE_PACKAGE_DIGEST_FILE" && -f "$FAKE_LINK_DIR/version-42.tsv" ]] || { printf 'version not prepared\n400'; exit 0; }
        printf '42\n' >"$FAKE_ACTIVE_VERSION_FILE" ;;
      */service/dummyservice/version/40/activate)
        grep -qx 40 "$FAKE_VERSION_FILE" || { printf 'version not prepared\n400'; exit 0; }
        printf '40\n' >"$FAKE_ACTIVE_VERSION_FILE" ;;
      */service/dummyservice/version/39/activate)
        grep -qx 39 "$FAKE_VERSION_FILE" || { printf 'version not prepared\n400'; exit 0; }
        printf '39\n' >"$FAKE_ACTIVE_VERSION_FILE" ;;
      */service/dummyservice/version/42/deactivate/staging)
        [[ "$(cat "$FAKE_STAGED_VERSION_FILE" 2>/dev/null || true)" == 42 ]] || { printf 'version not staged\n400'; exit 0; }
        : >"$FAKE_STAGED_VERSION_FILE" ;;
      *) printf 'unexpected mutation\n400'; exit 0;;
    esac
    printf '200'
    exit 0
  fi
  case "$url" in
    */resources/stores/kv\?limit=100)
      printf '{"data":[{"id":"KVPROD","name":"cache-prod"},{"id":"KVSTAGE","name":"cache-stage"}],"meta":{"next_cursor":null}}\n200' ;;
    */resources/stores/secret\?limit=100)
      printf '{"data":[{"id":"SECRETPROD","name":"credentials-prod"},{"id":"SECRETSTAGE","name":"credentials-stage"}],"meta":{"next_cursor":null}}\n200' ;;
    */service/dummyservice/version) printf '%s\n200' "$(version_list)" ;;
    */service/dummyservice/version/42/package)
      printf '{"service_id":"dummyservice","version":42,"metadata":{"files_hash":"%0128d"}}\n200' 0 ;;
    */service/dummyservice/version/*/domain\?include=staging_ips)
      printf '[{"name":"staging.example.com","staging_ip":"151.101.2.10"}]\n200' ;;
    */resources/stores/config/*/item/*) printf 'unexpected Config Store item read\n400' ;;
    *) printf 'unexpected fake API read\n404';;
  esac
  exit 0
fi

printf 'PROBE %s\n' "$*" >>"$FAKE_CALL_LOG"
printf 'PROBE-TOKEN=%s\n' "${FASTLY_API_TOKEN:+set}" >>"$FAKE_CALL_LOG"
if [[ -n "${FORCE_UNHEALTHY:-}" ]]; then echo 503; else echo 200; fi
SHIM
  chmod +x "$path"
}

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local runner_temp="${RUNNER_TEMP:?RUNNER_TEMP is required}"
  local action_dir path_dir downloads log state pinned stage archive sha expected_digest
  action_dir=$(cd -- "$SCRIPT_DIR/../../deploy-fastly" && pwd)
  path_dir="$workspace/fake-bin"
  downloads="$runner_temp/edgezero-action-tools/downloads"
  log="$workspace/fake-calls.log"
  state="$workspace/fake-fastly-state"

  mkdir -p "$path_dir" "$downloads" "$state/links" "$state/config-push"
  : >"$log"
  printf '40\n' >"$state/active-version"
  printf '39\n40\n' >"$state/versions"
  printf 'LINK_RUNTIME\tedgezero_runtime_env\tENVSEL1\tconfig_store\nLINK_CONFIG_PROD\tapp_config\tCONFIGPROD\tconfig_store\nLINK_KV_PROD\tcache\tKVPROD\tkv_store\nLINK_SECRET_PROD\tcredentials\tSECRETPROD\tsecret_store\n' >"$state/links/version-40.tsv"
  : >"$state/staged-version"
  : >"$state/package-digest"

  pinned=$(json_get "$action_dir/versions.json" fastly.version)
  stage=$(mktemp -d)
  write_fake_fastly "$stage/fastly" "$pinned"
  archive="$downloads/fastly-$pinned-linux-amd64.tar.gz"
  tar -C "$stage" -czf "$archive" fastly
  sha=$(sha256_file "$archive")
  local patched
  patched=$(mktemp)
  jq --arg url "file://$archive" --arg sha "$sha" '.fastly.linux_amd64.url = $url | .fastly.linux_amd64.sha256 = $sha' "$action_dir/versions.json" >"$patched"
  mv "$patched" "$action_dir/versions.json"
  write_fake_curl "$path_dir/curl"

  expected_digest=''
  [[ ! -f "$workspace/fixture-release/package.sha256" ]] || expected_digest=$(cat "$workspace/fixture-release/package.sha256")
  append_env FAKE_CALL_LOG "$log"
  append_env FAKE_ACTIVE_VERSION_FILE "$state/active-version"
  append_env FAKE_VERSION_FILE "$state/versions"
  append_env FAKE_STAGED_VERSION_FILE "$state/staged-version"
  append_env FAKE_LINK_DIR "$state/links"
  append_env FAKE_CONFIG_PUSH_DIR "$state/config-push"
  append_env FAKE_PACKAGE_DIGEST_FILE "$state/package-digest"
  append_env FAKE_EXPECTED_PACKAGE_DIGEST "$expected_digest"
  append_env PATH "$path_dir:$PATH"
}

main "$@"
