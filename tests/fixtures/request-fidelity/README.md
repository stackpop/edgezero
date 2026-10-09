# Request ingress runtime fixtures

Run from the repository root with Rust 1.95.0 and Node 24.12.0:

```sh
./scripts/test-request-fidelity.sh all
./scripts/test-request-fidelity.sh fastly --skip-build
```

`--skip-build` intentionally reuses existing fixture artifacts; it does not
prove source freshness. Use the default locked build for review evidence.

The isolated Cargo workspace and npm lock pin fixture dependencies. Install
Viceroy 0.17.0, Spin 4.0.0, worker-build 0.8.3 and wasm-bindgen CLI 0.2.122.
Wrangler 4.83.0 and its local workerd are installed by `npm ci`. Each runtime
uses its own target directory, an ephemeral loopback port, bounded readiness
and an owned process group that is cleaned up on success, failure or interruption.
No platform credentials or deployment are required.

The runner sends 56 fixed raw HTTP cases across Fastly, Cloudflare and Spin.
The client and process orchestration use an isolated native Rust harness with exact existing Tokio/serde_json/tempfile/TOML pins. Tokio is confined to this host test helper; it adds no core or WASM runtime dependency. Responses contain fixed booleans/counts rather than unrestricted request values.
`observations.json` records the pinned local runtimes' observed outcomes, and
subsequent runs compare them exactly. Inspect changes before updating it.
A passing regression run includes explicitly reported unsupported cases; it
does not establish consumer compliance or production wire preservation.
Axum's real TCP ingress cases live alongside its request converter tests.

Known gates:

- Two Cloudflare routing regressions send raw UTF-8 `/café` and `/a\b`
  directly to the pinned workerd, bypassing Wrangler's development proxy,
  which normalizes these targets before Worker invocation. A no-hook
  application must match `/caf%C3%A9` and `/a/b` while ingress metadata
  independently retains the raw runtime URL with unknown wire preservation.
- Fastly 0.12.1 lacks a safe borrowed original-target getter. Ingress reports
  `NotExposed`; a preceding native path shortcut can see a normalized target.
  Its canonical origin is recovered separately from the client runtime URI
  and one validated consistent Host, before native mutation.
  A snapshot probe changes the native URL/Host after capture and proves the
  original origin remains available through explicit-snapshot dispatch.
- Local workerd rejects extension methods before Worker invocation, normalizes
  dot segments and coalesces repeated fields. Raw FF becomes U+FFFD before Rust
  and is copied as runtime UTF-8 EF BF BD. Common header fidelity remains
  `Unknown`, since a transforming runtime does not prove any particular field
  changed. Valid wire UTF-8 C3 A9 and C4 80 are asserted byte-for-byte.
  Repeated Cookie values are checked for exact comma joining versus semicolon
  joining. Compound probes compare a valid-looking diagnostics value plus
  unrelated invalid FF against original valid EF BF BD: their Worker verdicts
  are identical. They do not implement or prove Trusted Server cookie parsing.
  Absolute-form and duplicate Content-Length requests return local 500 errors.
- Spin 4.0.0 rejects non-UTF-8 header values before component invocation.
- Global original field order is unavailable on every adapter. Local evidence
  cannot certify production runtime or TLS/client provenance guarantees.

The single [spec](../../../docs/superpowers/specs/2026-10-05-pre-dispatch-request-fidelity-design.md)
and [plan](../../../docs/superpowers/plans/2026-10-05-pre-dispatch-request-fidelity.md)
record the complete contract, verification and remaining upstream/consumer gates.
