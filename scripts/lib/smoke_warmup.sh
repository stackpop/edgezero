# shellcheck shell=bash
# Shared smoke warm-up: provisions per-adapter local state via the
# generated app-demo-cli so smoke scripts can boot emulators on fresh
# clones where Cloudflare/Fastly/Spin manifests are gitignored.
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
        cd "$DEMO_DIR" || exit 1
        cargo run --quiet -p app-demo-cli -- \
            provision --adapter "$adapter" --local
    )
}

# Register (for backup + restore) EVERY provision-owned local file/dir that
# smoke_warmup_provision_local -- and a smoke body's `config push --local` --
# can create or mutate for ADAPTER. This is the SINGLE SOURCE OF TRUTH for
# the provision-owned file set that pairs with the warm-up above: whenever
# provision learns to write a new gitignored local file, add it HERE and
# every smoke inherits the coverage, so no individual script can forget it
# and leave operator-local state changed after a run (success OR failure).
#
# All these paths are gitignored per-machine state (see the app-demo entries
# in the root .gitignore). Requires scripts/lib/smoke_backup.sh sourced first
# (uses `backup_in_tree`, which fail-closed-aborts if a capture fails).
# Callers MUST invoke this BEFORE arming their restore trap and BEFORE
# calling smoke_warmup_provision_local, so a failed capture aborts before any
# mutation.
smoke_backup_provision_local() {
    local adapter
    adapter="$(smoke_canonical_adapter "$1")"
    # `.edgezero/` is created by EVERY adapter's provision: it anchors the
    # cross-process `provision.lock`, plus Axum's `.env` and local-config
    # JSON. Back it up regardless of adapter so a non-Axum run can't leave a
    # stray `.edgezero/` (or clobber existing Axum state) behind.
    backup_in_tree "$DEMO_DIR/.edgezero"
    case "$adapter" in
        axum)
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-axum/axum.toml"
            ;;
        cloudflare)
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-cloudflare/wrangler.toml"
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-cloudflare/.wrangler"
            ;;
        fastly)
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"
            ;;
        spin)
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-spin/spin.toml"
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-spin/runtime-config.toml"
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-spin/.env"
            backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-spin/.spin"
            ;;
    esac
}
