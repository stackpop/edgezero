// Many manifest fields exist for downstream consumers (CLI, runtime
// adapters, etc.) but are unused inside the proc-macro itself, which only
// reads enough of the structure to generate routing. Allow `dead_code` so
// those fields don't trip warnings just because the macro doesn't touch them.
#![allow(
    dead_code,
    reason = "macro-side reads only the routing-relevant fields"
)]

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../edgezero-core/src/manifest.rs"
));
