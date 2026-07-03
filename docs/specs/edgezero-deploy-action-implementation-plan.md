# EdgeZero Deploy GitHub Action Implementation Plan

**Status:** Implemented in this worktree

**Spec:** `docs/specs/edgezero-deploy-github-action.md`

## Scope

Implement the Fastly-only v0 deploy action in the EdgeZero monorepo at `.github/actions/deploy/action.yml`.

The action deploys already checked-out application source. It does not perform checkout, Fastly staging, runtime config mutation, health checks, rollback, or provider-specific deployment metadata parsing.

## Implementation phases

1. **Action metadata**
   - Define required `adapter` input and v0 Fastly-only contract.
   - Add `working-directory`, `manifest`, `rust-toolchain`, `build-mode`, JSON `build-args`/`deploy-args`, `cache`, and typed Fastly inputs.
   - Expose `adapter`, `source-revision`, `edgezero-revision`, `provider-cli-version`, and `effective-build-mode` outputs.

2. **Input validation**
   - Reject missing/unsupported adapters before setup.
   - Reject Cloudflare, Spin, Axum, and unknown adapters in v0.
   - Validate exact booleans, build mode, JSON array arguments, string-only entries, and NUL bytes.
   - Reject Fastly deploy args that override service selection, auth, endpoint, profile, debug, or interactive behavior.

3. **Project resolution**
   - Canonicalize `working-directory` and optional `manifest` inside `github.workspace`.
   - Resolve source Git root and committed source revision.
   - Fail on dirty source.
   - Resolve Rust toolchain using explicit input, Rustup files, `.tool-versions`, then EdgeZero repo fallback.
   - Build exact cache key from OS, arch, Rust toolchain, target, EdgeZero revision, source revision, and lockfile hash.

4. **Tool installation**
   - Install application Rust toolchain and `wasm32-wasip1` target.
   - Set `RUSTUP_TOOLCHAIN` for subsequent application build/deploy commands.
   - Build action-owned `edgezero-cli` from the selected EdgeZero action commit with only Fastly CLI support.
   - Install pinned Fastly CLI from official release artifact and verify checksum.

5. **Execution**
   - Resolve Fastly `build-mode: auto` to `never`.
   - Run optional build without Fastly credentials.
   - Run deploy with Bash arrays, typed Fastly token, action-owned `--service-id`, action-owned `--non-interactive`, then safe caller deploy args.
   - Clear inherited Fastly auth aliases from every non-provider step and clear/re-export typed values in deploy.

6. **Caching and cleanup**
   - Restore cache only when `cache: true`.
   - Save cache only after successful execution and non-empty resolved key/path.
   - Cache only application Git root `target/`.
   - Clean action-owned state/tool directories with `if: always()`.

7. **Logging and docs**
   - Write non-sensitive summary fields.
   - Document full-SHA invocation, examples, inputs/outputs, credential scope, build behavior, cache behavior, and non-goals.

8. **Validation**
   - Script contract tests for validation, path confinement, toolchain parsing, cache keys, credential scoping, and build/deploy argv.
   - Actionlint, ShellCheck, zizmor, checksum metadata validation, docs validation, and composite smoke test workflow.
   - Workspace Rust tests, format, clippy, and feature checks.

## Known follow-up candidates

- Add provider-specific Fastly staging/rollback actions separately if needed.
- Add Cloudflare and Spin through new specs.
- Replace source-built CLI with prebuilt attested EdgeZero CLI binaries when available.
