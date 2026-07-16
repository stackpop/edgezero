# Shared smoke warm-up: provisions per-adapter local state via the
# generated app-demo-cli so smoke scripts can boot emulators on fresh
# clones where Cloudflare/Fastly/Spin manifests are gitignored (Task 33).
#
# Caller MUST set ROOT_DIR before sourcing this file (existing smoke
# bootstrap pattern; see scripts/smoke_test_config.sh:19).
#
# app-demo is excluded from the root workspace (Cargo.toml only lists
# in-tree crates; examples/app-demo is a separate workspace), so cargo
# commands run from inside DEMO_DIR. app-demo-cli has NO adapter
# features — adapter selection happens at the CLI arg level.
: "${ROOT_DIR:?ROOT_DIR must be set by the caller (existing smoke bootstrap)}"
DEMO_DIR="$ROOT_DIR/examples/app-demo"

# Normalise operator aliases to the canonical adapter name the manifest
# uses. `cf` is the historical shortcut smokes accept for Cloudflare.
smoke_canonical_adapter() {
    case "$1" in
        cf|cloudflare) echo "cloudflare" ;;
        *)             echo "$1" ;;
    esac
}

# Warm up the adapter's provision-owned local state so a fresh clone
# has usable manifests / .env / .dev.vars before the smoke tries to
# boot the emulator.
smoke_warmup_provision_local() {
    local adapter
    adapter="$(smoke_canonical_adapter "$1")"
    (
        cd "$DEMO_DIR"
        cargo run --quiet -p app-demo-cli -- \
            provision --adapter "$adapter" --local
    )
}
