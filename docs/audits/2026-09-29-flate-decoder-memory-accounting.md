# Flate Decoder Memory Accounting

This audit pins the flate decoder graph used by `edgezero-core` for HTTP
`deflate` and gzip response decoding. The audited graph is locked to:

- `async-compression 0.4.43`
- `compression-codecs 0.4.38`
- `compression-core 0.4.32`
- `flate2 1.1.9` with the Rust backend
- `miniz_oxide 0.8.9`
- `adler2 2.0.1`, `crc32fast 1.5.0`, and `simd-adler32 0.3.9`

The lockfile versions, registry source, checksums, metadata graph, and direct
normal dependencies are enforced by `scripts/check_flate_dependency_contract.mjs`
for both the root workspace and the excluded `examples/app-demo` workspace.

## Charge

The pinned `miniz_oxide` state reserves a 32 KiB sliding window and a 64 KiB
inflate state. The codec wrapper reserve is 4 KiB and the shared EdgeZero
stream output buffer is 8 KiB. The measured source subtotal is therefore
73,728 bytes before allocator metadata, fragmentation, task stacks, and host
buffers. `FLATE_DECODER_FIXED_CHARGE_BYTES` is pinned to 1 MiB, giving more
than 14x headroom over that subtotal while keeping the public policy uniform
for Brotli, gzip, and deflate.

The charge is a conservative guest-visible policy reservation, not a claim
about total process RSS. The documented outbound memory model continues to
exclude allocator metadata, fragmentation, adapter/SDK buffers, source-owned
chunks, output/body buffers, task stacks, and provider host allocations.
Those exclusions are why complete resource accounting remains a separate
capability.

Any dependency upgrade must repeat this audit, update the exact pins and
checksum contract, and re-evaluate the fixed charge before it is merged.
