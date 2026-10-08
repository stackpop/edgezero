# Brotli Decoder Memory Accounting Audit

This audit owns `BROTLI_DECODER_FIXED_CHARGE_BYTES = 16,777,216`. It derives an
allocation-payload subtotal for the source-owned non-window storage in EdgeZero's pinned Brotli
response decoder. It does not claim an exact total-heap bound. The ring buffer's `2^WBITS` bytes
are excluded from the fixed term because `brotli_decoder_memory_charge` charges them
independently; the fixed term below includes its additional 66-byte write-ahead/dictionary slack.

## Audited Graph

| Package               | Version  | Registry checksum                                                  |
| --------------------- | -------- | ------------------------------------------------------------------ |
| `alloc-no-stdlib`     | `2.0.4`  | `cc7bb162ec39d46ab1ca8c77bf72e890535becd1751bb45f64c597edb4c8c6b3` |
| `alloc-stdlib`        | `0.2.4`  | `0e76a019e91224d279006ff972f1e984179a6e9feb050adba6ce8274aef23195` |
| `async-compression`   | `0.4.43` | `3976abdc8fe7d1133d43d304afd42abdf5bc3e1319d263d223bde07b5efc4be8` |
| `brotli`              | `8.0.4`  | `5cc91aac060a7a1e25823bdccbfb6af1875b88f17c6daac97894eed8207166b3` |
| `brotli-decompressor` | `5.0.1`  | `5962523e1b92ce1b5e793d9169b9943eece10d39f62550bc04bb605d75b94924` |
| `compression-codecs`  | `0.4.38` | `ce2548391e9c1929c21bf6aa2680af86fe4c1b33e6cea9ac1cfeec0bd11218cf` |
| `compression-core`    | `0.4.32` | `cc14f565cf027a105f7a44ccf9e5b424348421a1d8952a8fc9d499d313107789` |

`compression-codecs` boxes one `brotli::BrotliState` and constructs it with the pinned
`alloc-stdlib::StandardAlloc` for byte, `u32`, and `HuffmanCode` storage.
`brotli-decompressor`'s `DecodeVarLenUint8` produces at most 255 before the caller adds one, so
every block-type or Huffman-tree count is at most 256. `BROTLI_HUFFMAN_MAX_TABLE_SIZE` is 1080,
and the pinned `#[repr(C)] HuffmanCode` occupies four bytes. The colocated core regression test
checks the compiled state/code sizes and EdgeZero bridge-buffer size. The lockfile checksum gate
protects private table, parser, and ring-layout assumptions that cannot be imported as public
constants.

## Audited Allocation-Payload Subtotal

| Allocation family                          |                    Maximum bytes |
| ------------------------------------------ | -------------------------------: |
| Boxed `BrotliState` payload reserve        |                           65,536 |
| Initial context-map Huffman table          |               `1080 * 4 = 4,320` |
| Block-type and block-length trees          |      `2 * 3 * 1080 * 4 = 25,920` |
| Literal context modes                      |                            `256` |
| Literal and distance context maps          |    `256 * 64 + 256 * 4 = 17,408` |
| Three Huffman-tree index arrays            |            `3 * 256 * 4 = 3,072` |
| Three Huffman-tree code arrays             | `3 * 256 * 1080 * 4 = 3,317,760` |
| EdgeZero decoder output buffer             |                          `8,192` |
| Ring write-ahead and dictionary-word slack |                   `42 + 24 = 66` |
| **Source-audited payload subtotal**        |                    **3,442,530** |

The public 16,777,216-byte fixed charge therefore leaves 13,334,686 bytes within the accounting
reservation above the source-audited payload subtotal. That remainder is deliberately reserved
for async wrapper/generator/replay objects, collection capacity, allocator metadata, and
fragmentation, but this audit does not claim an exact upper bound for those terms. The charge is a
conservative policy reservation, not a complete RSS certificate.

Encoded source chunks, decoded response retention, task stacks, adapter/SDK storage, and
provider-owned allocations are outside this decoder charge. Those terms are either separately
bounded or remain outside `outbound-complete-resource-accounting`, as documented in the
capability matrix.

## Change Gate

The dependency contract script pins every listed normal dependency, version, registry source, and
checksum in both workspace lockfiles. Any change to a pinned source, decoder construction path,
allocator, grammar limit, table-size constant, or bridge buffer requires repeating this source
audit before retaining or changing the fixed charge. Allocation measurements may be added as
regression evidence, but they do not replace this source-level accounting proof.
