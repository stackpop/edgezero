# Changelog

## Unreleased

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
