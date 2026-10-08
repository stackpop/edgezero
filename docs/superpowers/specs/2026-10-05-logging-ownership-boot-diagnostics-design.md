# Logging Ownership and Boot Diagnostics

## Contract

`Hooks::owns_logging() == true` means the application owns backend installation,
backend filters, and the `log` facade maximum. Adapters must not replace or
reconfigure that logger. Applications install it before the adapter entrypoint.

Core exposes a pure `resolve_logging_level(&EnvConfig) -> log::LevelFilter`
helper for `EDGEZERO__LOGGING__LEVEL`. It preserves the existing case-insensitive
parser and `Info` fallback for missing or invalid values; it does not install a
logger, read process environment, or change global filters. Manifest logging
resolution remains separate and is not implicitly applied by this helper.

`BOOT_LOG_TARGET` names `edgezero::boot`; `BOOT_LOG_LEVEL` is `Warn`. Adapter-owned
Fastly and Axum logging preserves errors and warnings on that target even when
ordinary logging is `Error` or `Off`. Each backend and the facade must admit the
maximum of the ordinary and boot levels, while the outer target-aware filter
continues to suppress ordinary records above their configured level. The boot
target is a category, not a new Fastly log endpoint.

Fastly `runtime_env_config` returns `FastlyRuntimeConfig { env, ... }`, hard-cutting
the former direct `EnvConfig` return. An unavailable optional runtime store sets
one boolean diagnostic flag. `emit_boot_diagnostics()` consumes its fixed warning
once after backend installation; missing backends do not gain implicit output.
`run_app` and `run_app_with_hooks` emit it even when application assembly returns
an error. The explicit-config runner does not read that runtime store.

Install adapter-owned logging before application configuration. Diagnostics
generated while resolving logger configuration must be retained as bounded
typed/static state and emitted after installation, not as an unbounded log
buffer. When an application owns logging, the same boot records use its backend
and filters; visibility depends on the application's policy.

Cloudflare and Spin logger initializers remain explicitly documented no-ops.
Their platform consoles do not automatically install a Rust `log` backend.
This change adds neither a logger dependency nor a new capability there.

The bundled CLI must not install its CLI logger before running the Axum demo.
Axum's `run_app_with_preflight` runs reusable startup checks after logging setup
and before application configuration or listener binding. The demo uses that
boundary for capability validation. Axum returns logger-setup errors rather than
silently using a different backend. CLI terminal errors use stderr regardless of
the runtime filter.
Generated adapter entrypoints continue delegating to reusable adapter code;
logging policy must not be duplicated in templates.

## Acceptance

- Resolver coverage includes every level, absent/invalid values, and no global
  logging side effects.
- Ordinary records retain their configured filter; boot `Error`/`Warn` survive
  ordinary `Error`/`Off`; boot `Info`/`Debug`/`Trace` remain suppressed.
- Fastly backend filtering and the facade ceiling cannot contradict the boot
  target policy.
- Configuration executes after adapter-owned logger setup; application-owned
  logger/filter state is not changed.
- Startup warning deferral is bounded and emits each diagnostic once.
- Direct Axum and bundled demo use the same runtime logging configuration.
- Documentation, generator output, and demo describe ownership accurately.

This is a logging API/documentation improvement, not an outbound correctness or
provider-certification prerequisite.
