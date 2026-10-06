# Changelog

## Unreleased

### Response EOF and batch finalization

Axum validates exact-length stream EOF before handing Hyper the final frame, and validates
zero-length streams before response handoff, under the original write deadline. Detached fallback
payloads remain sendable without a live completion attempt; HEAD suppression is preserved.

Outbound batches now require clean driver EOF before reporting `Completed`, including empty
drivers. Trailing failures or malformed events remain visible with previously collected slots.
Application configuration that changes adapter-selected platform metadata now fails assembly.

### Logging ownership and boot diagnostics

`owns_logging = true` now explicitly owns backend installation, backend filters, and the facade
maximum before entering an adapter runner. `resolve_logging_level(&EnvConfig)` shares runtime
level resolution. Managed Axum/Fastly loggers retain `edgezero::boot` warnings and errors at
`Warn` even when ordinary logging is `Error` or `Off`. Cloudflare/Spin initializers remain no-ops.

Fastly `runtime_env_config` now returns `FastlyRuntimeConfig` with an `env` field and bounded
deferred diagnostics. Custom callers must initialize logging and call `emit_boot_diagnostics()`;
the former direct `EnvConfig` return is removed, with no compatibility alias. Managed runners
initialize logging before application configuration. Axum now returns logger-setup errors instead
of ignoring them; preinstalled backends must use `owns_logging = true`.

Axum's `run_app_with_preflight` supports checks after logger setup and before configuration or
listener binding. The bundled demo uses it without first installing the CLI logger. Terminal CLI
errors remain visible on stderr regardless of runtime logging level.

### Outbound content encoding

`ContentEncoding::Passthrough` is now `Passthrough(PassthroughReason)`, and the public decoder
budget is now `max_decoder_bytes`. A single zlib-wrapped `deflate` response is decoded and its
compressed headers are removed, while stacked, unsupported, and malformed values retain their
typed passthrough reason. This is a hard API migration with no compatibility aliases.

### Platform resource metadata

Applications and adapter implementations must migrate to the current platform-fact and response
construction APIs. No compatibility aliases are retained.

| Removed or changed API                                      | Replacement                                                                               |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `MemoryCeilingSource`                                       | `PlatformResourceSource` inside `PlatformFact::Known`                                     |
| `MemoryCeiling::new(total, scope, stack, source)`           | `MemoryCeiling::new(total, scope, stack)` wrapped in `PlatformFact::known(value, source)` |
| `MemoryCeiling::source()`                                   | Match or inspect the enclosing `PlatformFact`                                             |
| `PlatformMetadata::new(Option<MemoryCeiling>)`              | `PlatformMetadata::new(memory, inbound_population, host_ingress_accounting)`              |
| `PlatformMetadata::memory_ceiling() -> Option<_>`           | `PlatformMetadata::memory_ceiling() -> PlatformFact<_>`                                   |
| `Adapter::memory_ceiling()`                                 | `Adapter::platform_metadata()`                                                            |
| overridable `Hooks::build_app()` and infallible `configure` | `App::build::<A>(platform)` plus fallible `Hooks::configure`                              |
| four-argument `OutboundResponse::new`                       | five-argument constructor requiring the application `MonotonicClock`                      |
