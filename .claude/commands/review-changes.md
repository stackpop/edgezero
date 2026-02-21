Review the current uncommitted changes as a staff engineer would. Run:

1. `git diff` to see all changes
2. `git diff --cached` to see staged changes
3. `git status` to see untracked files

Then provide a thorough code review covering:

- Correctness: are the changes logically sound?
- WASM compatibility: do changes avoid Tokio/runtime-specific deps in core/adapters?
- Convention compliance: matchit `{id}` syntax, `edgezero_core` imports, `#[action]` macros?
- Test coverage: are new code paths tested?
- Minimal scope: are there unnecessary changes beyond what was requested?

Be critical. Flag anything that would fail CI or violate project conventions from CLAUDE.md.
