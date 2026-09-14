#!/usr/bin/env node

import { readFileSync } from 'node:fs'

const capabilityPath = 'docs/guide/capabilities.md'
const outboundCorePath = 'crates/edgezero-core/src/outbound.rs'
const outboundSpecPath =
  'docs/superpowers/specs/2026-05-21-outbound-http-design.md'
const outboundImplementationIndexPath =
  'docs/superpowers/plans/2026-07-10-outbound-http-implementation.md'
const spinPhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase5-spin.md'
const cloudflarePhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase4-axum-cloudflare.md'
const fastlyPhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase6-fastly.md'
const migrationPhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase7-migration-docs.md'
const sidebarPath = 'docs/.vitepress/config.mts'
const expectedHeader = [
  'Capability',
  'Axum',
  'Cloudflare',
  'Fastly',
  'Spin',
]
const expectedIngressRows = [
  ['ingress-admission', 'Native', 'Native', 'Native', 'Native'],
  [
    'inbound-read-deadlines',
    'Native',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'raw-ingress-head-limits',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  [
    'raw-ingress-framing-validation',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
]
const expectedConfigRows = [
  [
    'config-read-allocation-bounds',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  [
    'config-read-deadlines',
    'BestEffort',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
]
const expectedOutboundRows = [
  ['outbound-http', 'Native', 'Native', 'BestEffort', 'Native'],
  [
    'outbound-complete-resource-accounting',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  ['outbound-header-fidelity', 'Native', 'BestEffort', 'Native', 'Native'],
  [
    'outbound-deadlines',
    'Native',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'outbound-flexible-phase-budget',
    'Native',
    'Native',
    'BestEffort',
    'BestEffort',
  ],
  ['send-all-slot-isolation', 'Native', 'Native', 'BestEffort', 'Native'],
  [
    'streamed-upload-deadlines',
    'Native',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'lazy-streamed-response-passthrough',
    'Native',
    'Native',
    'BestEffort',
    'BestEffort',
  ],
]
const expectedResponseEgressRows = [
  [
    'response-egress-abort',
    'BestEffort',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'response-egress-backpressure',
    'BestEffort',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'response-egress-completion',
    'BestEffort',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'response-write-deadlines',
    'BestEffort',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
]
const expectedLimitHeader = ['Control', 'Scope', 'Default']
const expectedLimitRows = [
  [
    'max_request_body_bytes',
    'Buffered or streamed request bytes',
    '8 MiB',
  ],
  [
    'max_encoded_response_bytes',
    'Upstream transport bytes before decoding',
    'Unset',
  ],
  [
    'max_decoded_response_bytes',
    'Identity or EdgeZero-decoded gzip/Brotli output',
    'Unset',
  ],
  [
    'max_response_bytes',
    'Final buffered response, including raw passthrough',
    '1 MiB',
  ],
  [
    'max_response_header_bytes',
    'Cumulative guest-visible header name/value bytes',
    'Unset',
  ],
  [
    'max_response_header_count',
    'Cumulative guest-visible header fields',
    'Unset',
  ],
  [
    'max_brotli_window_bits',
    'Brotli stream header checked before decoder allocation',
    '24',
  ],
  [
    'max_brotli_decoder_bytes',
    'Pinned policy charge for Brotli decoder state',
    '32 MiB',
  ],
  [
    'max_chunk_bytes',
    'Maximum emitted item size after decoding or passthrough',
    'Unset',
  ],
]

function fail(message) {
  process.stderr.write(`outbound docs contract: ${message}\n`)
  process.exit(1)
}

function cells(line) {
  const trimmed = line.trim()
  if (!trimmed.startsWith('|') || !trimmed.endsWith('|')) return null
  return trimmed
    .slice(1, -1)
    .split('|')
    .map((cell) => cell.trim())
}

function normalizeCapability(value) {
  return value.startsWith('`') && value.endsWith('`')
    ? value.slice(1, -1)
    : value
}

function normalizeSupport(value) {
  return value
    .replace(/\[\^[^\]]+\]$/u, '')
    .replace(/[¹²³⁴⁵⁶⁷⁸⁹]+$/u, '')
    .trim()
}

function readRustU64Constant(source, name) {
  const match = source.match(
    new RegExp(`pub const ${name}: u64 = ([^;]+);`, 'u'),
  )
  const expression = match?.[1]
  if (expression === undefined) {
    fail(`cannot find Rust constant ${name}`)
  }
  const factors = expression.split('*').map((factor) => factor.trim())
  if (!factors.every((factor) => /^[0-9][0-9_]*$/u.test(factor))) {
    fail(`Rust constant ${name} has an unsupported expression: ${expression}`)
  }
  return factors.reduce(
    (product, factor) => product * BigInt(factor.replaceAll('_', '')),
    1n,
  )
}

function formatBinaryBytes(bytes) {
  const mebibyte = 1024n * 1024n
  if (bytes % mebibyte === 0n) {
    return `${bytes / mebibyte} MiB`
  }
  return `${bytes} bytes`
}

let capabilitySource
try {
  capabilitySource = readFileSync(capabilityPath, 'utf8')
} catch (error) {
  fail(`cannot read ${capabilityPath}: ${error.message}`)
}

const lines = capabilitySource.split(/\r?\n/u)

function findMatrixHeader(sectionTitle, name) {
  const headingIndex = lines.findIndex(
    (line) => line.trim() === `## ${sectionTitle}`,
  )
  if (headingIndex === -1) {
    fail(`${name} capability section is missing`)
  }
  const nextHeadingIndex = lines.findIndex(
    (line, index) => index > headingIndex && line.startsWith('## '),
  )
  const sectionEnd = nextHeadingIndex === -1 ? lines.length : nextHeadingIndex
  const headerIndex = lines.findIndex(
    (line, index) =>
      index > headingIndex &&
      index < sectionEnd &&
      JSON.stringify(cells(line)) === JSON.stringify(expectedHeader),
  )
  if (headerIndex === -1) {
    fail(`${name} capability matrix is missing`)
  }
  return headerIndex
}

function readMatrix(headerIndex, name) {
  const separator = cells(lines[headerIndex + 1] ?? '')
  if (
    separator === null ||
    separator.length !== expectedHeader.length ||
    !separator.every((value) => /^:?-{3,}:?$/u.test(value))
  ) {
    fail(`${name} capability matrix is missing its five-column separator row`)
  }

  const actualRows = []
  for (let index = headerIndex + 2; index < lines.length; index += 1) {
    const row = cells(lines[index])
    if (row === null) break
    if (row.length !== expectedHeader.length) {
      fail(`${name} capability matrix row ${index + 1} has ${row.length} cells, expected 5`)
    }
    actualRows.push([
      normalizeCapability(row[0]),
      ...row.slice(1).map(normalizeSupport),
    ])
  }
  return actualRows
}

const matrices = [
  [
    'ingress',
    expectedIngressRows,
    readMatrix(findMatrixHeader('Ingress Matrix', 'ingress'), 'ingress'),
  ],
  [
    'config',
    expectedConfigRows,
    readMatrix(findMatrixHeader('Config Matrix', 'config'), 'config'),
  ],
  [
    'response egress',
    expectedResponseEgressRows,
    readMatrix(
      findMatrixHeader('Response Egress Matrix', 'response egress'),
      'response egress',
    ),
  ],
  [
    'outbound',
    expectedOutboundRows,
    readMatrix(findMatrixHeader('Outbound Matrix', 'outbound'), 'outbound'),
  ],
]
for (const [name, expectedRows, actualRows] of matrices) {
  if (JSON.stringify(actualRows) !== JSON.stringify(expectedRows)) {
    fail(
      `${name} capability matrix mismatch\nexpected=${JSON.stringify(expectedRows)}\nactual=${JSON.stringify(actualRows)}`,
    )
  }
}

let outboundSpecSource
try {
  outboundSpecSource = readFileSync(outboundSpecPath, 'utf8')
} catch (error) {
  fail(`cannot read ${outboundSpecPath}: ${error.message}`)
}

if (!outboundSpecSource.includes('## 2. Historical implementation baseline')) {
  fail(
    'outbound specification must label section 2 as a historical implementation baseline',
  )
}
if (outboundSpecSource.includes('## 2. Current state (summary)')) {
  fail(
    'outbound specification still labels its historical baseline as current state',
  )
}
if (outboundSpecSource.includes("Today's Fastly client")) {
  fail(
    'outbound specification still describes the historical Fastly client as current',
  )
}
for (const staleFragment of [
  'So the collapse today is',
  'The current adapter maps',
  'two real bugs to fix',
  'The current code (`proxy.rs',
  'crates/edgezero-adapter-spin/src/proxy.rs',
  '(`spin/proxy.rs`)',
]) {
  if (outboundSpecSource.includes(staleFragment)) {
    fail(
      `outbound specification contains stale implementation text: ${staleFragment}`,
    )
  }
}

const cloudflareExactDeadlineRecommendation =
  /\btarget(?:s|ing)?\s+(?:Axum\s+(?:or|and)\s+Cloudflare|Cloudflare\s+(?:or|and)\s+Axum)\b/u
if (cloudflareExactDeadlineRecommendation.test(outboundSpecSource)) {
  fail(
    'outbound specification recommends Cloudflare for exact deadlines despite its BestEffort capability',
  )
}

const currentDocumentation = [
  [outboundImplementationIndexPath, readFileSync(outboundImplementationIndexPath, 'utf8')],
  [cloudflarePhasePath, readFileSync(cloudflarePhasePath, 'utf8')],
  [spinPhasePath, readFileSync(spinPhasePath, 'utf8')],
  [fastlyPhasePath, readFileSync(fastlyPhasePath, 'utf8')],
  [migrationPhasePath, readFileSync(migrationPhasePath, 'utf8')],
  [outboundSpecPath, outboundSpecSource],
]
for (const [path, source] of currentDocumentation) {
  for (const staleVersion of [
    'Spin SDK 6.0.0',
    'spin-sdk v6.0.0',
    'Viceroy 0.17.0',
    'viceroy 0.17.0',
    'Fastly SDK 0.12.1',
    'Worker 0.8.3',
    'worker = "=0.8.3"',
    'worker --precise 0.8.3',
    "worker v0\\.8\\.3",
  ]) {
    if (source.includes(staleVersion)) {
      fail(`${path} contains stale runtime tooling: ${staleVersion}`)
    }
  }
}

const outboundImplementationIndexSource = currentDocumentation[0][1]
if (!outboundImplementationIndexSource.includes('Spin SDK 7 / WASI HTTP 0.3')) {
  fail('outbound implementation index must name the current Spin SDK 7 baseline')
}
if (
  outboundImplementationIndexSource.includes('| Executable:') ||
  outboundImplementationIndexSource.includes('Tasks 1-6 blocked') ||
  outboundImplementationIndexSource.includes('all downstream phases stay')
) {
  fail('outbound implementation index still presents implemented phases as pending')
}

const limitsHeadingIndex = lines.findIndex(
  (line) => line.trim() === '## Limits And Accounting',
)
if (limitsHeadingIndex === -1) {
  fail('limits and accounting section is missing')
}
const limitsSectionEnd = lines.findIndex(
  (line, index) => index > limitsHeadingIndex && line.startsWith('## '),
)
const limitsHeaderIndex = lines.findIndex(
  (line, index) =>
    index > limitsHeadingIndex &&
    (limitsSectionEnd === -1 || index < limitsSectionEnd) &&
    JSON.stringify(cells(line)) === JSON.stringify(expectedLimitHeader),
)
if (limitsHeaderIndex === -1) {
  fail('outbound limits table is missing')
}
const limitsSeparator = cells(lines[limitsHeaderIndex + 1] ?? '')
if (
  limitsSeparator === null ||
  limitsSeparator.length !== expectedLimitHeader.length ||
  !limitsSeparator.every((value) => /^:?-{3,}:?$/u.test(value))
) {
  fail('outbound limits table is missing its three-column separator row')
}
const actualLimitRows = []
for (let index = limitsHeaderIndex + 2; index < lines.length; index += 1) {
  const row = cells(lines[index])
  if (row === null) break
  if (row.length !== expectedLimitHeader.length) {
    fail(
      `outbound limits row ${index + 1} has ${row.length} cells, expected 3`,
    )
  }
  actualLimitRows.push([normalizeCapability(row[0]), row[1], row[2]])
}
if (JSON.stringify(actualLimitRows) !== JSON.stringify(expectedLimitRows)) {
  fail(
    `outbound limits mismatch\nexpected=${JSON.stringify(expectedLimitRows)}\nactual=${JSON.stringify(actualLimitRows)}`,
  )
}

let outboundCoreSource
try {
  outboundCoreSource = readFileSync(outboundCorePath, 'utf8')
} catch (error) {
  fail(`cannot read ${outboundCorePath}: ${error.message}`)
}
const documentedDefaults = new Map(
  actualLimitRows.map((row) => [row[0], row[2]]),
)
for (const [control, constant] of [
  ['max_request_body_bytes', 'DEFAULT_OUTBOUND_REQUEST_BODY_BYTES'],
  ['max_response_bytes', 'DEFAULT_MAX_RESPONSE_BYTES'],
  ['max_brotli_decoder_bytes', 'DEFAULT_MAX_BROTLI_DECODER_BYTES'],
]) {
  const rustDefault = formatBinaryBytes(
    readRustU64Constant(outboundCoreSource, constant),
  )
  const documentedDefault = documentedDefaults.get(control)
  if (documentedDefault !== rustDefault) {
    fail(
      `${control} default mismatch: Rust ${constant} is ${rustDefault}, documentation says ${documentedDefault}`,
    )
  }
}

const sidebarSource = readFileSync(sidebarPath, 'utf8')
const sidebarLinks = sidebarSource.match(/link:\s*['"]\/guide\/capabilities['"]/gu) ?? []
if (sidebarLinks.length !== 1) {
  fail(
    `expected exactly one /guide/capabilities sidebar link, found ${sidebarLinks.length}`,
  )
}
