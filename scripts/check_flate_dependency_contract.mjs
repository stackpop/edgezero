#!/usr/bin/env node

import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'

const expected = JSON.parse(
  readFileSync('scripts/flate_dependency_contract.json', 'utf8'),
)
const registrySource = 'registry+https://github.com/rust-lang/crates.io-index'

function fail(workspace, message) {
  process.stderr.write(`flate dependency contract (${workspace}): ${message}\n`)
  process.exitCode = 1
}

function metadata(workspace) {
  const output = execFileSync(
    'cargo',
    ['metadata', '--locked', '--offline', '--format-version', '1', '--all-features'],
    { cwd: workspace, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 },
  )
  return JSON.parse(output)
}

function lockValue(packageBlock, key) {
  return packageBlock.match(new RegExp(`^${key} = "([^"]+)"$`, 'mu'))?.[1]
}

function auditLockfile(workspace) {
  const lockPath = workspace === '.' ? 'Cargo.lock' : `${workspace}/Cargo.lock`
  const packageBlocks = readFileSync(lockPath, 'utf8')
    .split('[[package]]')
    .slice(1)

  for (const [name, version, checksum] of expected) {
    const matches = packageBlocks.filter(
      (block) => lockValue(block, 'name') === name,
    )
    if (matches.length !== 1) {
      fail(workspace, `expected one locked ${name} package, found ${matches.length}`)
      continue
    }
    const block = matches[0]
    if (
      lockValue(block, 'version') !== version ||
      lockValue(block, 'source') !== registrySource ||
      lockValue(block, 'checksum') !== checksum
    ) {
      fail(
        workspace,
        `locked ${name} source, version, or checksum differs from the audited contract`,
      )
    }
  }
}

function auditWorkflowFetchOrder() {
  const workflow = readFileSync('.github/workflows/test.yml', 'utf8')
  const lines = workflow.split('\n').map((line) => line.trim())
  const commandIndex = (command) =>
    lines.findIndex((line) => line === command || line === `run: ${command}`)
  const rootFetch = commandIndex('cargo fetch --locked')
  const demoFetch = commandIndex(
    'cargo fetch --locked --manifest-path examples/app-demo/Cargo.toml',
  )
  const audit = commandIndex('node scripts/check_flate_dependency_contract.mjs')

  if (
    rootFetch === -1 ||
    demoFetch === -1 ||
    audit === -1 ||
    rootFetch >= audit ||
    demoFetch >= audit
  ) {
    fail(
      'workflow',
      'root and app-demo locked fetches must run before the offline flate dependency audit',
    )
  }
}

function audit(workspace) {
  auditLockfile(workspace)
  const graph = metadata(workspace)
  const packagesById = new Map(graph.packages.map((pkg) => [pkg.id, pkg]))
  const nodesById = new Map(graph.resolve.nodes.map((node) => [node.id, node]))

  for (const [name, version] of expected) {
    const matches = graph.packages.filter((pkg) => pkg.name === name)
    if (matches.length !== 1) {
      fail(workspace, `expected one ${name} package, found ${matches.length}`)
      continue
    }
    if (matches[0].version !== version) {
      fail(
        workspace,
        `expected ${name} ${version}, resolved ${matches[0].version}`,
      )
    }
  }

  const core = graph.packages.find((pkg) => pkg.name === 'edgezero-core')
  if (core === undefined) {
    fail(workspace, 'edgezero-core package is missing')
    return
  }
  const coreNode = nodesById.get(core.id)
  if (coreNode === undefined) {
    fail(workspace, 'edgezero-core resolve node is missing')
    return
  }

  for (const [name, version] of expected) {
    const declarations = core.dependencies.filter(
      (dependency) => dependency.name === name && dependency.kind === null,
    )
    if (declarations.length !== 1 || declarations[0].req !== `=${version}`) {
      fail(
        workspace,
        `edgezero-core must declare exactly one normal ${name} dependency pinned as =${version}`,
      )
    }
    const direct = coreNode.dependencies
      .map((id) => packagesById.get(id))
      .find((pkg) => pkg?.name === name)
    if (direct === undefined) {
      fail(workspace, `edgezero-core does not pin ${name} as a normal dependency`)
    }
  }
}

auditWorkflowFetchOrder()
audit('.')
audit('examples/app-demo')

if (process.exitCode === undefined) {
  process.stdout.write('flate dependency contract: ok\n')
}
