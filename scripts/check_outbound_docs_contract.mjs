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
const expectedRows = [
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
const headerIndices = lines
  .map((line, index) => ({ index, row: cells(line) }))
  .filter(({ row }) => JSON.stringify(row) === JSON.stringify(expectedHeader))
  .map(({ index }) => index)

if (headerIndices.length !== 1) {
  fail(`expected exactly one capability matrix, found ${headerIndices.length}`)
}

const headerIndex = headerIndices[0]
const separator = cells(lines[headerIndex + 1] ?? '')
if (
  separator === null ||
  separator.length !== expectedHeader.length ||
  !separator.every((value) => /^:?-{3,}:?$/u.test(value))
) {
  fail('capability matrix is missing its five-column separator row')
}

const actualRows = []
for (let index = headerIndex + 2; index < lines.length; index += 1) {
  const row = cells(lines[index])
  if (row === null) break
  if (row.length !== expectedHeader.length) {
    fail(`capability matrix row ${index + 1} has ${row.length} cells, expected 5`)
  }
  actualRows.push([
    normalizeCapability(row[0]),
    ...row.slice(1).map(normalizeSupport),
  ])
}

if (JSON.stringify(actualRows) !== JSON.stringify(expectedRows)) {
  fail(
    `capability matrix mismatch\nexpected=${JSON.stringify(expectedRows)}\nactual=${JSON.stringify(actualRows)}`,
  )
}

const sidebarSource = readFileSync(sidebarPath, 'utf8')
const sidebarLinks = sidebarSource.match(/link:\s*['"]\/guide\/capabilities['"]/gu) ?? []
if (sidebarLinks.length !== 1) {
  fail(
    `expected exactly one /guide/capabilities sidebar link, found ${sidebarLinks.length}`,
  )
}
