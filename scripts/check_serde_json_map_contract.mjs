#!/usr/bin/env node

import { execFileSync } from 'node:child_process'

const tree = execFileSync(
  'cargo',
  [
    'tree',
    '--locked',
    '--offline',
    '--package',
    'edgezero-cli',
    '--no-default-features',
    '--edges',
    'features,no-dev',
    '--invert',
    'serde_json',
  ],
  { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 },
)

const forbiddenFeatures = [
  'serde_json feature "indexmap"',
  'serde_json feature "preserve_order"',
]

for (const feature of forbiddenFeatures) {
  if (tree.includes(feature)) {
    process.stderr.write(
      `serde_json map contract: no-CLI graph enables ${feature}\n`,
    )
    process.exitCode = 1
  }
}

if (process.exitCode === undefined) {
  process.stdout.write('serde_json map contract: ok\n')
}
