#!/usr/bin/env node

import { readFileSync } from 'node:fs'

const capabilityPath = 'docs/guide/capabilities.md'
const adapterOverviewPath = 'docs/guide/adapters/overview.md'
const changelogPath = 'CHANGELOG.md'
const handlersGuidePath = 'docs/guide/handlers.md'
const axumGuidePath = 'docs/guide/adapters/axum.md'
const cloudflareGuidePath = 'docs/guide/adapters/cloudflare.md'
const proxyGuidePath = 'docs/guide/proxying.md'
const routingGuidePath = 'docs/guide/routing.md'
const fastlyGuidePath = 'docs/guide/adapters/fastly.md'
const scaffoldReadmePath =
  'crates/edgezero-cli/src/templates/root/README.md.hbs'
const outboundCorePath = 'crates/edgezero-core/src/outbound.rs'
const compressionCorePath = 'crates/edgezero-core/src/compression.rs'
const outboundAdapterPaths = [
  'crates/edgezero-adapter-axum/src/outbound.rs',
  'crates/edgezero-adapter-cloudflare/src/outbound.rs',
  'crates/edgezero-adapter-fastly/src/outbound.rs',
  'crates/edgezero-adapter-spin/src/outbound.rs',
]
const templateHandlersPath =
  'crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs'
const demoHandlersPath =
  'examples/app-demo/crates/app-demo-core/src/handlers.rs'
const observationCutoffSourcePaths = [
  outboundCorePath,
  'crates/edgezero-core/src/context.rs',
  'crates/edgezero-core/src/time.rs',
  ...outboundAdapterPaths,
  ...['axum', 'cloudflare', 'fastly', 'spin'].map(
    (adapter) => `crates/edgezero-adapter-${adapter}/tests/contract.rs`,
  ),
  templateHandlersPath,
  demoHandlersPath,
  proxyGuidePath,
]
const brotliAuditPath =
  'docs/audits/2026-09-27-brotli-decoder-memory-accounting.md'
const outboundSpecPath =
  'docs/superpowers/specs/2026-05-21-outbound-http-design.md'
const platformMetadataDesignPath =
  'docs/superpowers/specs/2026-09-28-platform-resource-metadata-design.md'
const inboundSpecPath =
  'docs/superpowers/specs/2026-08-22-inbound-body-design.md'
const responseEgressSpecPath =
  'docs/superpowers/specs/2026-09-08-response-egress-design.md'
const outboundImplementationIndexPath =
  'docs/superpowers/plans/2026-07-10-outbound-http-implementation.md'
const outboundBatchTerminationPlanPath =
  'docs/superpowers/plans/2026-09-16-outbound-batch-termination.md'
const outboundReviewHardeningPlanPath =
  'docs/superpowers/plans/2026-09-10-outbound-http-review-hardening.md'
const spinPhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase5-spin.md'
const cloudflarePhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase4-axum-cloudflare.md'
const fastlyPhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase6-fastly.md'
const migrationPhasePath =
  'docs/superpowers/plans/2026-09-06-outbound-http-phase7-migration-docs.md'
const sidebarPath = 'docs/.vitepress/config.mts'
const expectedHeader = ['Capability', 'Axum', 'Cloudflare', 'Fastly', 'Spin']
const expectedIngressRows = [
  ['ingress-admission', 'Native', 'Native', 'Native', 'Native'],
  [
    'ingress-admission-abort',
    'Native',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
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
    'outbound-authority-override',
    'Native',
    'BestEffort',
    'Native',
    'Unsupported',
  ],
  [
    'outbound-batch-cancellation',
    'Native',
    'BestEffort',
    'BestEffort',
    'BestEffort',
  ],
  [
    'outbound-batch-completion-order',
    'Native',
    'Native',
    'BestEffort',
    'Native',
  ],
  ['outbound-batch-slot-isolation', 'Native', 'Native', 'BestEffort', 'Native'],
  ['outbound-cache-bypass', 'Native', 'Native', 'Native', 'Native'],
  [
    'outbound-complete-resource-accounting',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  ['outbound-header-fidelity', 'Native', 'BestEffort', 'Native', 'Native'],
  ['outbound-deadlines', 'Native', 'BestEffort', 'BestEffort', 'BestEffort'],
  [
    'outbound-flexible-phase-budget',
    'Native',
    'Native',
    'BestEffort',
    'BestEffort',
  ],
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
  ['max_request_body_bytes', 'Buffered or streamed request bytes', '8 MiB'],
  [
    'max_encoded_response_bytes',
    'Upstream transport bytes before decoding',
    'Unset',
  ],
  [
    'max_decoded_response_bytes',
    'Identity or EdgeZero-decoded gzip/deflate/Brotli output',
    'Unset',
  ],
  [
    'max_response_bytes',
    'Final buffered response, including raw passthrough',
    '1 MiB',
  ],
  [
    'max_response_header_bytes',
    'Adapter-visible upstream header name/value bytes before normalization, plus `x-edgezero-proxy`',
    'Unset',
  ],
  [
    'max_response_header_count',
    'Adapter-visible upstream header fields before normalization, plus `x-edgezero-proxy`',
    'Unset',
  ],
  [
    'max_brotli_window_bits',
    'Brotli stream header checked before decoder allocation',
    '24',
  ],
  [
    'max_decoder_bytes',
    'Pinned policy charge for Brotli/gzip/deflate decoder state',
    '32 MiB',
  ],
  [
    'max_chunk_bytes',
    'Maximum emitted item size after decoding or passthrough',
    'Unset',
  ],
]
const expectedMemoryHeader = [
  'Target',
  'Primary ceiling',
  'Separate stack',
  'Scope',
  'Memory source / reason',
  'Live inbound requests',
  'Population source / reason',
  'Host ingress/framing charge',
  'Accounting source / reason',
]
const expectedMemoryRows = [
  [
    'Axum',
    'Unknown',
    'Unknown',
    'Unknown',
    'Unknown: Operator configured',
    'Unknown',
    'Unknown: Operator configured',
    'Unknown',
    'Unknown: Operator configured',
  ],
  [
    'Cloudflare Workers',
    '128000000 bytes (128 MB)',
    'None',
    'Per instance',
    'Platform limit: Cloudflare Workers',
    'Unknown',
    'Unknown: Provider unpublished',
    'Unknown',
    'Unknown: Provider unpublished',
  ],
  [
    'Fastly Compute',
    '128000000 bytes (128 MB)',
    '1000000 bytes (1 MB)',
    'Per execution',
    'Platform limit: Fastly Compute',
    '1',
    'Platform limit: Fastly Compute',
    'Unknown',
    'Unknown: Provider unpublished',
  ],
  [
    'Spin (generic)',
    'Unknown',
    'Unknown',
    'Unknown',
    'Unknown: Runtime configured',
    'Unknown',
    'Unknown: Runtime configured',
    'Unknown',
    'Unknown: Runtime configured',
  ],
  [
    'Akamai Functions (Spin)',
    '134217728 bytes (128 MiB)',
    'None',
    'Per execution',
    'Hosted default: Akamai Functions',
    '1',
    'Hosted default: Akamai Functions',
    'Unknown',
    'Unknown: Provider unpublished',
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

const qualifiedDeadline = String.raw`(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*Deadline`
const optionalDeadline = String.raw`Option\s*<\s*${qualifiedDeadline}\s*>`
const deadlineType = String.raw`(?:${qualifiedDeadline}|${optionalDeadline})`

function maskObservationCutoffNonCode(source) {
  const masked = source.split('')
  const blank = (start, end) => {
    for (let index = start; index < end; index += 1) {
      if (!/[\r\n]/u.test(masked[index])) masked[index] = ' '
    }
  }
  let index = 0
  while (index < source.length) {
    if (source.startsWith('//', index)) {
      const newline = source.indexOf('\n', index + 2)
      const end = newline === -1 ? source.length : newline
      blank(index, end)
      index = end
      continue
    }
    if (source.startsWith('/*', index)) {
      let depth = 1
      let end = index + 2
      while (end < source.length && depth > 0) {
        if (source.startsWith('/*', end)) {
          depth += 1
          end += 2
        } else if (source.startsWith('*/', end)) {
          depth -= 1
          end += 2
        } else {
          end += 1
        }
      }
      blank(index, end)
      index = end
      continue
    }
    const rest = source.slice(index)
    const rawStart = rest.match(/^(?:b|c)?r(#{0,255})"/u)
    if (rawStart !== null) {
      const delimiter = `"${rawStart[1]}`
      const closing = source.indexOf(delimiter, index + rawStart[0].length)
      const end = closing === -1 ? source.length : closing + delimiter.length
      blank(index, end)
      index = end
      continue
    }
    const quoted = rest.match(
      /^(?:(?:b|c)?"(?:\\[\s\S]|[^"\\])*"|b?'(?:\\(?:u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2}|.)|[^'\\\r\n])')/u,
    )
    if (quoted !== null) {
      blank(index, index + quoted[0].length)
      index += quoted[0].length
      continue
    }
    index += 1
  }
  return masked.join('')
}

function markdownFenceOpening(line) {
  const match = /^ {0,3}(`{3,}|~{3,})[ \t]*([^\r\n]*)$/u.exec(line)
  if (match === null) return null
  if (match[1][0] === '`' && match[2].includes('`')) return null
  const language = match[2]
    .trim()
    .split(/[:,\s]/u, 1)[0]
    .toLowerCase()
  return {
    marker: match[1][0],
    minimumLength: match[1].length,
    rust: language === 'rust' || language === 'rs',
  }
}

function markdownFenceClosing(line, fence) {
  let index = 0
  while (index < 3 && line[index] === ' ') index += 1
  let markerLength = 0
  while (line[index + markerLength] === fence.marker) markerLength += 1
  return (
    markerLength >= fence.minimumLength &&
    line.slice(index + markerLength).trim() === ''
  )
}

function markdownViews(source) {
  const prose = source.split('')
  const rust = source.split('')
  const blank = (view) => {
    for (let index = 0; index < source.length; index += 1) {
      if (!/[\r\n]/u.test(source[index])) view[index] = ' '
    }
  }
  const copy = (view, start, end) => {
    for (let index = start; index < end; index += 1) view[index] = source[index]
  }
  blank(prose)
  blank(rust)

  let fence = null
  let lineStart = 0
  while (lineStart < source.length) {
    const newline = source.indexOf('\n', lineStart)
    const lineEnd = newline === -1 ? source.length : newline
    const nextLine = newline === -1 ? source.length : newline + 1
    const line = source.slice(lineStart, lineEnd).replace(/\r$/u, '')

    if (fence === null) {
      const opening = markdownFenceOpening(line)
      if (opening === null) copy(prose, lineStart, nextLine)
      else {
        fence = {
          ...opening,
          contentStart: newline === -1 ? source.length : newline + 1,
        }
      }
    } else if (markdownFenceClosing(line, fence)) {
      if (fence.rust) copy(rust, fence.contentStart, lineStart)
      fence = null
    }

    if (newline === -1) break
    lineStart = nextLine
  }

  if (fence?.rust) copy(rust, fence.contentStart, source.length)
  return { prose: prose.join(''), rust: rust.join('') }
}

function closingDelimiter(source, open) {
  const pairs = new Map([
    ['(', ')'],
    ['[', ']'],
    ['{', '}'],
  ])
  const stack = [pairs.get(source[open])]
  for (let index = open + 1; index < source.length; index += 1) {
    if (pairs.has(source[index])) stack.push(pairs.get(source[index]))
    else if (source[index] === stack.at(-1)) {
      stack.pop()
      if (stack.length === 0) return index
    }
  }
  return -1
}

function topLevelArguments(source, open, close) {
  const argumentsList = []
  let start = open + 1
  for (let index = start; index <= close; index += 1) {
    if (index === close || source[index] === ',') {
      const argument = source.slice(start, index).trim()
      if (argument !== '') argumentsList.push(argument)
      start = index + 1
    } else if ('([{'.includes(source[index])) {
      index = closingDelimiter(source, index)
      if (index === -1) return []
    }
  }
  return argumentsList
}

function signatureContractError(
  source,
  name,
  count,
  typeShape,
  unusedCount = 0,
) {
  const code = maskObservationCutoffNonCode(source)
  const signatures =
    code.match(new RegExp(String.raw`\bfn\s+${name}\s*\([^)]*\)`, 'gu')) ?? []
  const expectedType =
    typeShape === 'optional' ? optionalDeadline : qualifiedDeadline
  const parameterCount = (parameter) =>
    signatures.filter((signature) =>
      new RegExp(
        String.raw`(?:^|,)\s*${parameter}\s*:\s*${expectedType}\s*(?=,|\))`,
        'u',
      ).test(signature.slice(signature.indexOf('(') + 1)),
    ).length
  const staleParameter = new RegExp(
    String.raw`(?:^|,)\s*(?:cutoff|_cutoff|batch_cutoff)\s*:\s*${deadlineType}\s*(?=,|\))`,
    'u',
  )
  return signatures.some((signature) => staleParameter.test(signature)) ||
    signatures.length !== count ||
    parameterCount('observation_cutoff') !== count - unusedCount ||
    parameterCount('_observation_cutoff') !== unusedCount
    ? `${name} must use ${typeShape} observation_cutoff: Deadline in ${count} signature(s)`
    : null
}

function exampleCodeError(code) {
  const staleName = String.raw`(?:cutoff|_cutoff|batch_cutoff)`
  const staleBinding = new RegExp(
    String.raw`^\s*let\s+(?:mut\s+)?${staleName}\b`,
    'mu',
  )
  if (staleBinding.test(code)) return 'a stale cutoff binding'
  const batchCall = /\b(?:start_batch_until|send_all_until)\s*\(/gu
  for (const match of code.matchAll(batchCall)) {
    const open = match.index + match[0].lastIndexOf('(')
    const close = closingDelimiter(code, open)
    if (close === -1) continue
    const finalArgument = topLevelArguments(code, open, close).at(-1)
    if (new RegExp(String.raw`^${staleName}$`, 'u').test(finalArgument ?? '')) {
      return 'a stale batch cutoff argument'
    }
  }
  return null
}

function exampleContractError(path, source) {
  if (!path.endsWith('.md') && !path.endsWith('.mdx')) {
    return exampleCodeError(maskObservationCutoffNonCode(source))
  }
  const views = markdownViews(source)
  return (
    exampleCodeError(views.prose) ??
    exampleCodeError(maskObservationCutoffNonCode(views.rust))
  )
}

function runObservationCutoffContractSelfTests() {
  const bareCutoff = ['cut', 'off'].join('')
  const ignoredCutoff = `_${bareCutoff}`
  const batchCutoff = `batch_${bareCutoff}`
  for (const [parameters, typeShape, valid] of [
    ['observation_cutoff: Deadline', 'plain', true],
    ['observation_cutoff: edgezero_core :: Deadline', 'plain', true],
    [
      'observation_cutoff: Option < edgezero_core :: Deadline >',
      'optional',
      true,
    ],
    ['observation_cutoff: Option<Deadline>', 'plain', false],
    ['observation_cutoff: Deadline', 'optional', false],
    [`${bareCutoff}: Deadline`, 'plain', false],
    [`${ignoredCutoff}: edgezero_core::Deadline`, 'plain', false],
    [`${batchCutoff}: Option < Deadline >`, 'optional', false],
    [`${bareCutoff}: Deadline, observation_cutoff: Deadline`, 'plain', false],
  ]) {
    const source = `fn dispatch_budget(request: Request, ${parameters}) {}`
    const passed =
      signatureContractError(source, 'dispatch_budget', 1, typeShape) === null
    if (passed !== valid) {
      fail(`observation cutoff signature self-test failed for ${parameters}`)
    }
  }
  for (const [name, source, valid] of [
    ['mutable binding', `let mut ${bareCutoff} = deadline();`, false],
    ['ignored binding', `let ${ignoredCutoff} = deadline();`, false],
    ['batch binding', `let ${batchCutoff} = deadline();`, false],
    [
      'start call',
      `client.start_batch_until(vec![request()], ${bareCutoff});`,
      false,
    ],
    ['ignored call', `client.send_all_until(x, ${ignoredCutoff});`, false],
    ['send call', `client.send_all_until(x, ${batchCutoff});`, false],
    [
      'multiline trailing call',
      `client.start_batch_until(\n    make_requests(),\n    ${bareCutoff},\n);`,
      false,
    ],
    [
      'nested cutoff',
      `client.start_batch_until(make_requests(x, ${bareCutoff}), observation_cutoff);`,
      true,
    ],
    ['comment', `// let mut ${bareCutoff} = deadline();`, true],
    [
      'string',
      `const TEXT: &str = "send_all_until(requests, ${bareCutoff})";`,
      true,
    ],
    ['budget source', 'let source = BudgetSource::BatchCutoff;', true],
    ['constructor', 'let batch = OutboundBatch::cutoff(2);', true],
  ]) {
    if ((exampleContractError('fixture.rs', source) === null) !== valid) {
      fail(`observation cutoff example self-test failed for ${name}`)
    }
  }
  if (
    exampleContractError(
      'fixture.md',
      `Prose: send_all_until(requests, ${bareCutoff})\n` +
        '```rust\nlet observation_cutoff = deadline();\n```',
    ) === null
  ) {
    fail('observation cutoff self-test missed Markdown prose')
  }
  if (
    exampleContractError(
      'fixture.md',
      `\`\`\`rust,no_run\nlet ${bareCutoff} = deadline();\n\`\`\``,
    ) === null
  ) {
    fail('observation cutoff self-test missed a modified Rust fence')
  }
  if (
    exampleContractError(
      'fixture.md',
      `\`\`\`rust\`bad\nlet ${bareCutoff} = deadline();\n\`\`\``,
    ) === null
  ) {
    fail('observation cutoff self-test treated an invalid backtick fence as a fence')
  }
  for (const [name, fence] of [
    [
      'tilde colon modifier',
      `~~~rust:no_run\nlet ${bareCutoff} = deadline();\n~~~~`,
    ],
    [
      'indented space modifier',
      `   \`\`\`rust no_run\nlet ${bareCutoff} = deadline();\n   \`\`\``,
    ],
    [
      'variable-length rs fence',
      `\`\`\`\`rs:ignore\nlet ${bareCutoff} = deadline();\n\`\`\`\`\``,
    ],
  ]) {
    if (exampleContractError('fixture.md', fence) === null) {
      fail(`observation cutoff self-test missed ${name}`)
    }
  }
  for (const [name, fence] of [
    ['non-Rust fence', `\`\`\`text\nlet ${bareCutoff} = deadline();\n\`\`\``],
    [
      'Rust fence comments and literals',
      `\`\`\`rust\n// let ${bareCutoff} = deadline();\n` +
        `const TEXT: &str = "let ${bareCutoff} = deadline();";\n` +
        `const RAW: &str = r#"let ${bareCutoff} = deadline();"#;\n\`\`\``,
    ],
  ]) {
    if (exampleContractError('fixture.md', fence) !== null) {
      fail(`observation cutoff self-test scanned ${name}`)
    }
  }
  for (const [name, source] of [
    [
      'block comment',
      `fn dispatch_budget(x: Request, observation_cutoff: Deadline) {}\n/*\nlet ${bareCutoff} = deadline();\nfn dispatch_budget(x: Request, ${bareCutoff}: Deadline) {}\n*/`,
    ],
    [
      'raw string',
      `fn dispatch_budget(x: Request, observation_cutoff: Deadline) {}\nconst TEXT: &str = r#"\nlet ${bareCutoff} = deadline();\nfn dispatch_budget(x: Request, ${bareCutoff}: Deadline) {}\n"#;`,
    ],
  ]) {
    if (
      signatureContractError(source, 'dispatch_budget', 1, 'plain') !== null ||
      exampleContractError('fixture.rs', source) !== null
    ) {
      fail(`observation cutoff self-test rejected ${name}`)
    }
  }
}

function observationCutoffContract(outboundCoreSource) {
  const publicRustdoc = outboundCoreSource.match(
    /(?<rustdoc>(?:\s*\/\/\/[^\n]*\n)+)\s*fn start_batch_until\(/u,
  )?.groups?.rustdoc
  if (publicRustdoc === undefined) fail('missing start_batch_until Rustdoc')
  const normalizedPublicRustdoc = publicRustdoc
    .replaceAll(/\s*\/\/\/\s?/gu, ' ')
    .replaceAll(/\s+/gu, ' ')
    .trim()
  for (const requiredFragment of [
    'The observation cutoff stops batch observation regardless of any remaining per-request budget or later per-request deadline.',
    'Passing an earlier observation cutoff intentionally leaves unresolved slots.',
  ]) {
    if (!normalizedPublicRustdoc.includes(requiredFragment)) {
      fail(`public batch Rustdoc is missing: ${requiredFragment}`)
    }
  }

  const signatureContracts = [
    [outboundCorePath, 'start_batch_until', 3, 'plain', 1],
    [outboundCorePath, 'send_all_until', 1, 'plain'],
    [outboundCorePath, 'finish_batch_item', 1, 'plain'],
    ['crates/edgezero-core/src/context.rs', 'start_batch_until', 1, 'plain', 1],
    ['crates/edgezero-core/src/time.rs', 'dispatch_budget', 1, 'optional'],
    ...outboundAdapterPaths.flatMap((path) => [
      [path, 'prepare_batch', 1, 'plain'],
      [path, 'prepare_validated', 1, 'optional'],
      [path, 'start_batch_until', 1, 'plain'],
    ]),
    [outboundAdapterPaths[2], 'finish_fastly_batch_observation', 1, 'plain'],
    [templateHandlersPath, 'start_batch_until', 1, 'plain'],
    [demoHandlersPath, 'start_batch_until', 1, 'plain'],
  ]
  for (const [
    path,
    name,
    count,
    typeShape,
    unusedCount,
  ] of signatureContracts) {
    const source = readFileSync(path, 'utf8')
    const error = signatureContractError(
      source,
      name,
      count,
      typeShape,
      unusedCount,
    )
    if (error !== null) fail(`${path}: ${error}`)
  }
  for (const path of observationCutoffSourcePaths) {
    const error = exampleContractError(path, readFileSync(path, 'utf8'))
    if (error !== null) fail(`${path} contains ${error}`)
  }
}

runObservationCutoffContractSelfTests()
if (process.argv.includes('--observation-cutoff-self-test')) {
  process.exit(0)
}

let capabilitySource
try {
  capabilitySource = readFileSync(capabilityPath, 'utf8')
} catch (error) {
  fail(`cannot read ${capabilityPath}: ${error.message}`)
}

const lines = capabilitySource.split(/\r?\n/u)

function readExactTable(sectionTitle, header, name) {
  const headingIndex = lines.findIndex(
    (line) => line.trim() === `## ${sectionTitle}`,
  )
  if (headingIndex === -1) fail(`${name} section is missing`)
  const nextHeadingIndex = lines.findIndex(
    (line, index) => index > headingIndex && line.startsWith('## '),
  )
  const sectionEnd = nextHeadingIndex === -1 ? lines.length : nextHeadingIndex
  const headerIndex = lines.findIndex(
    (line, index) =>
      index > headingIndex &&
      index < sectionEnd &&
      JSON.stringify(cells(line)) === JSON.stringify(header),
  )
  if (headerIndex === -1) fail(`${name} table is missing`)
  const separator = cells(lines[headerIndex + 1] ?? '')
  if (
    separator === null ||
    separator.length !== header.length ||
    !separator.every((value) => /^:?-{3,}:?$/u.test(value))
  ) {
    fail(`${name} table has an invalid separator row`)
  }
  const rows = []
  for (let index = headerIndex + 2; index < sectionEnd; index += 1) {
    const row = cells(lines[index])
    if (row === null) break
    if (row.length !== header.length) {
      fail(
        `${name} row ${index + 1} has ${row.length} cells, expected ${header.length}`,
      )
    }
    rows.push(row)
  }
  return rows
}

const actualMemoryRows = readExactTable(
  'Platform Memory Ceilings',
  expectedMemoryHeader,
  'platform memory',
)
if (JSON.stringify(actualMemoryRows) !== JSON.stringify(expectedMemoryRows)) {
  fail(
    `platform memory mismatch\nexpected=${JSON.stringify(expectedMemoryRows)}\nactual=${JSON.stringify(actualMemoryRows)}`,
  )
}
const normalizedCapabilitySource = capabilitySource.replaceAll(/\s+/gu, ' ')
for (const requiredFragment of [
  'six simultaneous outbound connections',
  'it is not an inbound population bound',
  'host parser, HPACK, or framing allocations',
  'only `Fits` is a complete validation result',
]) {
  if (!normalizedCapabilitySource.includes(requiredFragment)) {
    fail(`platform memory narrative is missing: ${requiredFragment}`)
  }
}

function tableAfterHeading(source, heading, header) {
  const sourceLines = source.split(/\r?\n/u)
  const headingIndex = sourceLines.findIndex((line) => line.trim() === heading)
  if (headingIndex === -1) fail(`missing heading: ${heading}`)
  const nextHeadingOffset = sourceLines
    .slice(headingIndex + 1)
    .findIndex((line) => /^#{1,6}\s/u.test(line.trim()))
  const sectionEnd =
    nextHeadingOffset === -1
      ? sourceLines.length
      : headingIndex + 1 + nextHeadingOffset
  const headerIndex = sourceLines.findIndex(
    (line, index) =>
      index > headingIndex &&
      index < sectionEnd &&
      JSON.stringify(cells(line)) === JSON.stringify(header),
  )
  if (headerIndex === -1) fail(`missing table under ${heading}`)
  const separator = cells(sourceLines[headerIndex + 1] ?? '')
  if (
    separator === null ||
    separator.length !== header.length ||
    !separator.every((cell) => /^:?-{3,}:?$/u.test(cell))
  ) {
    fail(`invalid table separator under ${heading}`)
  }
  const rows = []
  for (let index = headerIndex + 2; index < sectionEnd; index += 1) {
    const row = cells(sourceLines[index])
    if (row === null) break
    if (row.length !== header.length) {
      fail(`invalid table row width under ${heading}`)
    }
    rows.push(row)
  }
  return rows
}

const migrationHeader = ['Removed or changed API', 'Replacement']
const designMigrationRows = tableAfterHeading(
  readFileSync(platformMetadataDesignPath, 'utf8'),
  '## Migration record',
  migrationHeader,
)
const changelogMigrationRows = tableAfterHeading(
  readFileSync(changelogPath, 'utf8'),
  '### Platform resource metadata',
  migrationHeader,
)
if (
  designMigrationRows.length !== 8 ||
  JSON.stringify(changelogMigrationRows) !== JSON.stringify(designMigrationRows)
) {
  fail(
    'Unreleased changelog must contain the focused design migration table exactly',
  )
}
if (
  !readFileSync(adapterOverviewPath, 'utf8').includes(
    'CHANGELOG.md#platform-resource-metadata',
  )
) {
  fail('adapter overview must link to the Unreleased migration table')
}

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
      fail(
        `${name} capability matrix row ${index + 1} has ${row.length} cells, expected 5`,
      )
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

const hardCutSurfaces = [
  [capabilityPath, capabilitySource],
  [handlersGuidePath, readFileSync(handlersGuidePath, 'utf8')],
  [proxyGuidePath, readFileSync(proxyGuidePath, 'utf8')],
  [fastlyGuidePath, readFileSync(fastlyGuidePath, 'utf8')],
  [scaffoldReadmePath, readFileSync(scaffoldReadmePath, 'utf8')],
]
for (const [path, source] of hardCutSurfaces) {
  for (const staleFragment of [
    'HttpClient::send_all`',
    '.send_all(',
    'send-all-slot-isolation',
    'run_app_with_request_extensions',
  ]) {
    if (source.includes(staleFragment)) {
      fail(`${path} contains removed outbound API text: ${staleFragment}`)
    }
  }
}
for (const staleFragment of [
  'harvests response bodies in input order',
  'harvests responses in input order',
]) {
  if (capabilitySource.includes(staleFragment)) {
    fail(
      `${capabilityPath} contains stale Fastly batch behavior: ${staleFragment}`,
    )
  }
}
const handlersGuideSource = hardCutSurfaces[1][1]
for (const requiredFragment of [
  'fn configure_app(app: &mut edgezero_core::app::App) -> Result<(), EdgeError>',
  'completion: ResponseEgressCompletion::empty()',
  'DetachedResponseEgressDecision::Send {',
  'deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),',
  'ResponseEgressCompletion::new(move |report| ... )',
  'Use `left.join(right)`',
  '`AdmissionDecision::Abort`',
  '`App::set_detached_response_egress_decision_factory`',
  'app.set_error_response_renderer(|error|',
  '`DetachedResponseEgressDecision::Abort`',
  'Every `Send` deadline is mandatory and absolute',
  '`Abort` creates no response or egress attempt and invokes no completion callback',
  'sorted, deduplicated `Allow` field',
  'Effective `config_out_of_date` responses retain `Retry-After: 60`',
  'status returned by the callback is ignored',
  'never belongs in response extensions',
]) {
  if (!handlersGuideSource.includes(requiredFragment)) {
    fail(
      `${handlersGuidePath} is missing lifecycle contract: ${requiredFragment}`,
    )
  }
}
const cloudflareGuideSource = readFileSync(cloudflareGuidePath, 'utf8')
if (
  !cloudflareGuideSource.includes(
    'EdgeZeroApp::build::<App>(CLOUDFLARE_PLATFORM)',
  ) ||
  cloudflareGuideSource.includes('build_app')
) {
  fail(
    `${cloudflareGuidePath} must construct manual applications with Cloudflare platform metadata`,
  )
}
const axumGuideSource = readFileSync(axumGuidePath, 'utf8')
if (
  !axumGuideSource.includes(
    'EdgeZeroApp::build::<App>(edgezero_adapter_axum::AXUM_PLATFORM)',
  ) ||
  axumGuideSource.includes('build_app')
) {
  fail(
    `${axumGuidePath} must construct manual applications with Axum platform metadata`,
  )
}
const routingGuideSource = readFileSync(routingGuidePath, 'utf8')
if (
  !routingGuideSource.includes('App::build::<A>(platform)') ||
  routingGuideSource.includes('build_app')
) {
  fail(
    `${routingGuidePath} must document the platform-aware application builder`,
  )
}
for (const [path, source] of [
  [capabilityPath, capabilitySource],
  [handlersGuidePath, handlersGuideSource],
]) {
  if (source.includes('build_app_for_platform')) {
    fail(`${path} contains the removed application builder name`)
  }
}
if (
  !hardCutSurfaces[2][1].includes('start_batch_until') ||
  !hardCutSurfaces[2][1].includes('send_all_until')
) {
  fail('outbound guide must document completion-order and ordered batch access')
}
for (const requiredFragment of [
  'OutboundBatchNext::Item',
  'OutboundBatchNext::Finished',
  'OutboundBatchTermination::Completed',
  'OutboundBatchTermination::Cutoff',
  'OutboundBatchFailure',
  'send_all_until(requests, observation_cutoff)',
]) {
  if (!hardCutSurfaces[2][1].includes(requiredFragment)) {
    fail(
      `${proxyGuidePath} is missing typed batch contract: ${requiredFragment}`,
    )
  }
}
if (
  !hardCutSurfaces[2][1].includes('.deadline_after(Duration::from_secs(2))') ||
  hardCutSurfaces[2][1].includes(
    'let observation_cutoff = Deadline::after(Duration::from_secs(2))',
  )
) {
  fail(`${proxyGuidePath} must anchor batch cutoffs to the request clock`)
}
if (
  !hardCutSurfaces[3][1].includes('run_app_with_hooks') ||
  !hardCutSurfaces[3][1].includes('send_request_with_hooks')
) {
  fail('Fastly guide must document the closed request/response lifecycle APIs')
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
  'explicit low-level constructors use a documented default clock',
  'explicit default-clock constructors may select the',
  'Preserve default-clock constructors for low-level use',
  'low-level defaults',
]) {
  if (outboundSpecSource.includes(staleFragment)) {
    fail(
      `outbound specification contains removed clock behavior: ${staleFragment}`,
    )
  }
}
const budgetSourceDefinitions = outboundSpecSource.match(
  /pub enum BudgetSource\s*\{/gu,
)
if (budgetSourceDefinitions?.length !== 1) {
  fail('outbound specification must define BudgetSource exactly once')
}
for (const staleFragment of [
  'So the collapse today is',
  'The current adapter maps',
  'two real bugs to fix',
  'The current code (`proxy.rs',
  'crates/edgezero-adapter-spin/src/proxy.rs',
  '(`spin/proxy.rs`)',
  'harvests every successfully dispatched `PendingRequest` via blocking `wait()`/`poll()`',
  'cold registration, serial harvest, and streamed-upload cooperative checks',
  'The target-neutral batch probe exercises both still-pending and later-ready harvest branches',
  'send_all_preflight_precedence_and_indices',
  'send_all_dispatches_every_slot_before_wait',
  'ordered harvest',
  'harvest-order',
  'serial harvest',
  'send_one_validated',
  'exact eight outbound',
  'all eight outbound',
  'Fastly send_all_until adapter overhead',
  'pub async fn next(&mut self) -> Option<OutboundBatchItem>',
  'pub async fn collect(self) -> OutboundBatchResults',
  'OutboundBatch::from_stream',
  'Result<OutboundBatchResults, EdgeError>',
  'let Ok((selection, metadata)) = select_pending_slot(&mut pending) else',
  'private failure event',
  'four of the eight outbound capabilities',
  'the eight outbound capabilities',
  'all eight rows/support values',
  'allocation-tracking adversarial',
]) {
  if (outboundSpecSource.includes(staleFragment)) {
    fail(
      `outbound specification contains stale implementation text: ${staleFragment}`,
    )
  }
}
for (const requiredFragment of [
  '`BudgetSource::BatchCutoff` attribution alone never emits `OutboundBatchDriverEvent::Cutoff`',
  'monotonic clock shows that the absolute observation cutoff has expired; equality is expired',
  "An earlier Fastly phase timeout remains that slot's terminal `GatewayTimeout` item",
  'pub enum OutboundBatchDriverEvent',
  'pub enum OutboundBatchTermination',
  'pub enum OutboundBatchNext',
  'pub struct OutboundBatchFailure',
  'pub fn cutoff(slot_count: usize) -> Self',
  'pub fn finish_batch_item(',
  'pub fn from_driver<StreamValue>',
  'Result<OutboundBatchResults, OutboundBatchFailure>',
  'premature driver EOF',
  'duplicate or out-of-range',
]) {
  if (!outboundSpecSource.includes(requiredFragment)) {
    fail(
      `outbound specification is missing typed batch contract: ${requiredFragment}`,
    )
  }
}
if (
  !/terminal samples follow\s+poll-observation order in the shared monotonic clock domain/u.test(
    outboundSpecSource,
  )
) {
  fail(
    'outbound specification is missing the monotonic terminal-observation ordering proof',
  )
}

const cloudflareExactDeadlineRecommendation =
  /\btarget(?:s|ing)?\s+(?:Axum\s+(?:or|and)\s+Cloudflare|Cloudflare\s+(?:or|and)\s+Axum)\b/u
if (cloudflareExactDeadlineRecommendation.test(outboundSpecSource)) {
  fail(
    'outbound specification recommends Cloudflare for exact deadlines despite its BestEffort capability',
  )
}

const inboundSpecSource = readFileSync(inboundSpecPath, 'utf8')
for (const requiredFragment of [
  '    Abort,',
  'pub enum DetachedResponseEgressDecision',
  '    Send {',
  '        completion: ResponseEgressCompletion,',
  '        deadline: Deadline,',
  'DetachedResponseEgressDecision::Send {',
  'deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),',
  'The deadline is mandatory, absolute',
  'Joined completions run left then right with the same borrowed report at most once',
  'Factory `Abort` creates no response or attempt',
  'make no transport-observed reset claim',
  'ResponseEgressCompletion::empty()',
  'Hooks::configure(&mut App) -> Result<(), EdgeError>',
  '`App::set_error_response_renderer(Fn(EdgeError) -> Response)`',
  '`Vec<Method>` rather than flattening protocol data into display text',
  'custom renderer cannot reinterpret the error status',
  'the canonical `Allow` field',
  '`Retry-After: 60` for every effective `config_out_of_date` outcome after custom rendering',
]) {
  if (!inboundSpecSource.includes(requiredFragment)) {
    fail(
      `${inboundSpecPath} is missing lifecycle contract: ${requiredFragment}`,
    )
  }
}
for (const staleFragment of [
  'current Tower adapter nevertheless drives',
  'block_in_place` plus a nested runtime `block_on',
  'MethodNotAllowed   { allowed: String',
  'Tokio cannot cancel that blocking closure',
]) {
  if (inboundSpecSource.includes(staleFragment)) {
    fail(
      `inbound specification contains removed Axum bridge text: ${staleFragment}`,
    )
  }
}

const outboundReviewHardeningPlanSource = readFileSync(
  outboundReviewHardeningPlanPath,
  'utf8',
)
for (const staleFragment of [
  'Preserve default-clock constructors for low-level use',
  'low-level defaults',
]) {
  if (outboundReviewHardeningPlanSource.includes(staleFragment)) {
    fail(`${outboundReviewHardeningPlanPath} contains removed clock behavior`)
  }
}

const currentDocumentation = [
  [
    outboundImplementationIndexPath,
    readFileSync(outboundImplementationIndexPath, 'utf8'),
  ],
  [
    outboundBatchTerminationPlanPath,
    readFileSync(outboundBatchTerminationPlanPath, 'utf8'),
  ],
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
    'Fastly 0.12.1',
    'fastly v0.12.1',
    'fastly = "=0.12.1"',
    'fastly --precise 0.12.1',
    'Worker 0.8.3',
    'worker = "=0.8.3"',
    'worker --precise 0.8.3',
    'worker v0.8.3',
  ]) {
    if (source.includes(staleVersion)) {
      fail(`${path} contains stale runtime tooling: ${staleVersion}`)
    }
  }
}

const outboundImplementationIndexSource = currentDocumentation[0][1]
if (!outboundImplementationIndexSource.includes('Spin SDK 7 / WASI HTTP 0.3')) {
  fail(
    'outbound implementation index must name the current Spin SDK 7 baseline',
  )
}
if (
  outboundImplementationIndexSource.includes('| Executable:') ||
  outboundImplementationIndexSource.includes('Tasks 1-6 blocked') ||
  outboundImplementationIndexSource.includes('all downstream phases stay') ||
  outboundImplementationIndexSource.includes(
    'exactly seven **outbound** capabilities',
  ) ||
  outboundImplementationIndexSource.includes(
    'block_in_place` + `Handle::block_on',
  )
) {
  fail('outbound implementation index contains stale implementation state')
}

let brotliAuditSource
try {
  brotliAuditSource = readFileSync(brotliAuditPath, 'utf8')
} catch (error) {
  fail(`cannot read ${brotliAuditPath}: ${error.message}`)
}
const normalizedBrotliAuditSource = brotliAuditSource.replace(/\s+/gu, ' ')
const expectedBrotliDependencies = JSON.parse(
  readFileSync('scripts/brotli_dependency_contract.json', 'utf8'),
)
const auditedBrotliDependencies = new Map(
  [
    ...brotliAuditSource.matchAll(
      /^\| `([^`]+)`\s+\| `([^`]+)`\s+\| `([0-9a-f]{64})` \|$/gmu,
    ),
  ].map((match) => [match[1], [match[2], match[3]]]),
)
if (auditedBrotliDependencies.size !== expectedBrotliDependencies.length) {
  fail(`${brotliAuditPath} must contain exactly the pinned dependency graph`)
}
for (const [name, version, checksum] of expectedBrotliDependencies) {
  const audited = auditedBrotliDependencies.get(name)
  if (audited?.[0] !== version || audited?.[1] !== checksum) {
    fail(
      `${brotliAuditPath} has stale version or checksum evidence for ${name}`,
    )
  }
}
for (const requiredFragment of [
  '`brotli-decompressor`',
  '`5.0.1`',
  '`compression-codecs`',
  '`0.4.38`',
  '`alloc-stdlib`',
  '`0.2.4`',
  '3 * 256 * 1080 * 4',
  '3,442,530',
  '16,777,216',
  'Source-audited payload subtotal',
  "The ring buffer's `2^WBITS` bytes are excluded from the fixed term",
]) {
  if (!normalizedBrotliAuditSource.includes(requiredFragment)) {
    fail(`${brotliAuditPath} is missing audited evidence: ${requiredFragment}`)
  }
}

const outboundBatchTerminationPlanSource = currentDocumentation[1][1]
if (outboundBatchTerminationPlanSource.includes('send_all_until(...).await?')) {
  fail(
    `${outboundBatchTerminationPlanPath} contains the stale batch collector error conversion`,
  )
}
if (
  outboundBatchTerminationPlanSource.includes('doc-hidden adapter driver event')
) {
  fail(
    `${outboundBatchTerminationPlanPath} still describes the public adapter driver protocol as doc-hidden`,
  )
}

const cloudflarePhaseSource = currentDocumentation[2][1]
for (const staleFragment of [
  "Cloudflare's Native timing claim",
  'Cloudflare: Native for HTTP, deadlines',
  '| `outbound-deadlines` | Native | Native |',
  '| `streamed-upload-deadlines` | Native | Native |',
]) {
  if (cloudflarePhaseSource.includes(staleFragment)) {
    fail(
      `${cloudflarePhasePath} contains stale Cloudflare capability text: ${staleFragment}`,
    )
  }
}

for (const [path, source] of [
  [cloudflarePhasePath, currentDocumentation[2][1]],
  [spinPhasePath, currentDocumentation[3][1]],
  [fastlyPhasePath, currentDocumentation[4][1]],
]) {
  if (!source.includes('**Superseded batch/lifecycle API:**')) {
    fail(`${path} does not mark its removed batch API as superseded`)
  }
}

const migrationPhaseSource = currentDocumentation[5][1]
for (const currentRow of [
  '| `outbound-deadlines` | Native | BestEffort | BestEffort | BestEffort |',
  '| `streamed-upload-deadlines` | Native | BestEffort | BestEffort | BestEffort |',
  '| `lazy-streamed-response-passthrough` | Native | Native | BestEffort | BestEffort |',
]) {
  if (!migrationPhaseSource.includes(currentRow)) {
    fail(
      `${migrationPhasePath} is missing current capability row: ${currentRow}`,
    )
  }
}

for (const dependency of [
  'async-compression 0.4.43',
  'adler2 = "=2.0.1"',
  'brotli 8.0.4',
  'brotli-decompressor 5.0.1',
  'compression-codecs 0.4.38',
  'compression-core 0.4.32',
  'crc32fast = "=1.5.0"',
  'flate2 = "=1.1.9"',
  'miniz_oxide = "=0.8.9"',
  'simd-adler32 = "=0.3.9"',
]) {
  if (!outboundSpecSource.includes(`\`${dependency}\``)) {
    fail(
      `outbound specification is missing audited dependency pin ${dependency}`,
    )
  }
}
if (
  !outboundSpecSource.includes(
    "plus EdgeZero's synthetic `x-edgezero-proxy` marker",
  )
) {
  fail(
    'outbound specification does not include the proxy header in response caps',
  )
}

const responseEgressSpecSource = readFileSync(responseEgressSpecPath, 'utf8')
for (const requiredFragment of [
  'pub struct ResponseEgressDeadline { /* private */ }',
  'pub const fn at(deadline: Deadline) -> Self;',
  'pub const fn after(duration: Duration) -> Self;',
  'egress-start-relative deadline',
  'resolves `after(duration)` from that',
  'exact sample before invoking the callback',
  'retired ambiguous `new()` constructor and unresolved',
  'pub struct ResponseEgressCompletion',
  'pub enum DetachedResponseEgressDecision',
  '    Send {',
  '        completion: ResponseEgressCompletion,',
  '        deadline: Deadline,',
  'DetachedResponseEgressDecision::Send {',
  'deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),',
  'mandatory absolute deadline',
  'pub fn join(self, other: Self) -> Self;',
  '`ResponseEgressCompletion::join` consumes a left and right completion',
  'the same borrowed `ResponseEgressReport`; the joined owner executes at most once',
  'existing non-response abort/error boundary',
  'Normalized pre-admission validation invokes the detached-egress decision factory exactly once',
  '`Abort` constructs no response or attempt and invokes no completion, observer, handler, middleware, or body poll',
  'ResponseEgressCompletion::empty()',
  'not attached to a `Response` and never enters `http::Extensions`',
  'ResponseEgressHead::application_deadline()',
  'queue-through-egress',
]) {
  if (!responseEgressSpecSource.includes(requiredFragment)) {
    fail(
      `${responseEgressSpecPath} is missing application deadline contract: ${requiredFragment}`,
    )
  }
}

if (
  responseEgressSpecSource.includes(
    'Normalized pre-admission validation obtains exactly one completion',
  )
) {
  fail(
    'response-egress evidence still claims every normalized rejection has a completion',
  )
}

for (const requiredFragment of [
  'detached-egress decision factory',
  'this does not promote their `BestEffort` capability cells',
]) {
  if (!capabilitySource.includes(requiredFragment)) {
    fail(
      `${capabilityPath} is missing detached-ingress abort caveat: ${requiredFragment}`,
    )
  }
}

const responseEgressHeadingIndex = lines.findIndex(
  (line) => line.trim() === '## Response Egress Matrix',
)
if (responseEgressHeadingIndex === -1) {
  fail(`${capabilityPath} is missing the response-egress section`)
}
const nextCapabilityHeadingIndex = lines.findIndex(
  (line, index) => index > responseEgressHeadingIndex && line.startsWith('## '),
)
const responseEgressSectionEnd =
  nextCapabilityHeadingIndex === -1 ? lines.length : nextCapabilityHeadingIndex
const responseEgressSection = lines
  .slice(responseEgressHeadingIndex, responseEgressSectionEnd)
  .join('\n')
for (const requiredFragment of [
  'The boundaries remain `BestEffort` because none proves end-client receipt:',
  'Provider-owned buffering, SDK allocations, and bytes beyond each stated acceptance boundary are',
]) {
  if (!responseEgressSection.includes(requiredFragment)) {
    fail(
      `${capabilityPath} response-egress section is missing caveat: ${requiredFragment}`,
    )
  }
}

function responseEgressProviderBullet(provider) {
  const bulletIndex = lines.findIndex(
    (line, index) =>
      index > responseEgressHeadingIndex &&
      index < responseEgressSectionEnd &&
      line.startsWith(`- ${provider} `),
  )
  if (bulletIndex === -1) {
    fail(`${capabilityPath} is missing the ${provider} response-egress caveat`)
  }
  const bullet = [lines[bulletIndex]]
  for (
    let index = bulletIndex + 1;
    index < responseEgressSectionEnd;
    index += 1
  ) {
    if (!lines[index].startsWith('  ')) break
    bullet.push(lines[index].trim())
  }
  return bullet.join(' ')
}

const responseEgressProviderCaveats = new Map([
  [
    'Axum',
    [
      'commits when Hyper accepts the response head',
      'closes the owned HTTP/1 connection on deadline',
      'reports clean source EOF as `HostHandoff`',
    ],
  ],
  [
    'Cloudflare',
    [
      'awaits JavaScript writer promises',
      'races request abort/deadline signals',
      'coordinator alive with `waitUntil`',
      'deployed disconnect and completion timing remain unproved',
    ],
  ],
  [
    'Fastly',
    [
      'accounts synchronous body-handle writes through `stream_to_client` close',
      'cannot preempt a blocked source poll or hostcall',
      'loses the body handle if `finish` itself fails',
    ],
  ],
  [
    'Spin',
    [
      'owns the WASI response/body/result writers through handoff',
      'races pending operations against its timer',
      'host teardown and network completion remain provider-observable only',
    ],
  ],
])
for (const [provider, requiredFragments] of responseEgressProviderCaveats) {
  const bullet = responseEgressProviderBullet(provider)
  for (const requiredFragment of requiredFragments) {
    if (!bullet.includes(requiredFragment)) {
      fail(
        `${capabilityPath} ${provider} response-egress caveat is missing: ${requiredFragment}`,
      )
    }
  }
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
    fail(`outbound limits row ${index + 1} has ${row.length} cells, expected 3`)
  }
  actualLimitRows.push([normalizeCapability(row[0]), row[1], row[2]])
}
if (JSON.stringify(actualLimitRows) !== JSON.stringify(expectedLimitRows)) {
  fail(
    `outbound limits mismatch\nexpected=${JSON.stringify(expectedLimitRows)}\nactual=${JSON.stringify(actualLimitRows)}`,
  )
}
for (const requiredFragment of [
  'including fields later stripped',
  "charge EdgeZero's synthetic proxy marker",
]) {
  if (!capabilitySource.includes(requiredFragment)) {
    fail(
      `${capabilityPath} is missing the response-header accounting rule: ${requiredFragment}`,
    )
  }
}

let outboundCoreSource
try {
  outboundCoreSource = readFileSync(outboundCorePath, 'utf8')
} catch (error) {
  fail(`cannot read ${outboundCorePath}: ${error.message}`)
}
let compressionCoreSource
try {
  compressionCoreSource = readFileSync(compressionCorePath, 'utf8')
} catch (error) {
  fail(`cannot read ${compressionCorePath}: ${error.message}`)
}
for (const requiredFragment of [
  'Deflate,',
  'Passthrough(PassthroughReason)',
  'pub enum PassthroughReason',
  'PassthroughReason::Stacked',
  'PassthroughReason::Malformed',
  'PassthroughReason::Unsupported(token)',
  'pub fn decode_deflate_stream',
]) {
  if (!compressionCoreSource.includes(requiredFragment)) {
    fail(`${compressionCorePath} is missing the content-encoding contract: ${requiredFragment}`)
  }
}
for (const requiredFragment of [
  'single `deflate`',
  'Passthrough `Unsupported(token)`',
  'Passthrough `Stacked`',
  'Passthrough `Malformed`',
  'decode_deflate_stream',
]) {
  if (!outboundSpecSource.includes(requiredFragment)) {
    fail(`${outboundSpecPath} is missing the content-encoding documentation: ${requiredFragment}`)
  }
}
observationCutoffContract(outboundCoreSource)
if (
  !/#\[non_exhaustive\]\s*pub enum OutboundBatchDriverEvent\s*\{/u.test(
    outboundCoreSource,
  )
) {
  fail('OutboundBatchDriverEvent must remain non-exhaustive')
}
const retiredClockConstructor = ['new_with_', 'monotonic_clock'].join('')
if (outboundCoreSource.includes(retiredClockConstructor)) {
  fail('OutboundResponse must not retain the hidden clock-paired constructor')
}
const outboundResponseConstructor =
  /impl OutboundResponse \{[\s\S]*?pub fn new\(\s*request_method: Method,\s*status: StatusCode,\s*headers: HeaderMap,\s*body: Body,\s*monotonic_clock: MonotonicClock,\s*\) -> Self \{/u
if (!outboundResponseConstructor.test(outboundCoreSource)) {
  fail('OutboundResponse::new must require the application monotonic clock')
}
for (const adapterPath of outboundAdapterPaths) {
  const adapterSource = readFileSync(adapterPath, 'utf8')
  const constructorCount =
    adapterSource.match(/OutboundResponse::new\(/gu)?.length ?? 0
  const pairedClockCount =
    adapterSource.match(/response_clock,\s*\)\)/gu)?.length ?? 0
  if (
    constructorCount === 0 ||
    constructorCount !== pairedClockCount ||
    !adapterSource.includes('let response_clock = clock.clone();')
  ) {
    fail(
      `${adapterPath} must clone the client clock and pass it to every outbound response`,
    )
  }
  if (!adapterSource.includes('converted_response_retains_injected_clock')) {
    fail(`${adapterPath} must behaviorally test the returned response clock`)
  }
}
const documentedDefaults = new Map(
  actualLimitRows.map((row) => [row[0], row[2]]),
)
for (const [control, constant] of [
  ['max_request_body_bytes', 'DEFAULT_OUTBOUND_REQUEST_BODY_BYTES'],
  ['max_response_bytes', 'DEFAULT_MAX_RESPONSE_BYTES'],
  ['max_decoder_bytes', 'DEFAULT_MAX_DECODER_BYTES'],
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
const sidebarLinks =
  sidebarSource.match(/link:\s*['"]\/guide\/capabilities['"]/gu) ?? []
if (sidebarLinks.length !== 1) {
  fail(
    `expected exactly one /guide/capabilities sidebar link, found ${sidebarLinks.length}`,
  )
}
