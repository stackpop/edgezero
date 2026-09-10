#!/usr/bin/env node

import { readFileSync } from 'node:fs'

const capabilityPath = 'docs/guide/capabilities.md'
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
  ['outbound-deadlines', 'Native', 'Native', 'BestEffort', 'BestEffort'],
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
    'Native',
    'BestEffort',
    'BestEffort',
  ],
  [
    'lazy-streamed-response-passthrough',
    'BestEffort',
    'Native',
    'BestEffort',
    'BestEffort',
  ],
]
const expectedResponseEgressRows = [
  [
    'response-egress-abort',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  [
    'response-egress-backpressure',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  [
    'response-egress-completion',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
  ],
  [
    'response-write-deadlines',
    'Unsupported',
    'Unsupported',
    'Unsupported',
    'Unsupported',
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

const sidebarSource = readFileSync(sidebarPath, 'utf8')
const sidebarLinks = sidebarSource.match(/link:\s*['"]\/guide\/capabilities['"]/gu) ?? []
if (sidebarLinks.length !== 1) {
  fail(
    `expected exactly one /guide/capabilities sidebar link, found ${sidebarLinks.length}`,
  )
}
