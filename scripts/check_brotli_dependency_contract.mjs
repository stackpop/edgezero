#!/usr/bin/env node

import { execFileSync } from 'node:child_process'

const expected = new Map([
  ['async-compression', '0.4.43'],
  ['brotli', '8.0.4'],
  ['brotli-decompressor', '5.0.1'],
  ['compression-codecs', '0.4.38'],
  ['compression-core', '0.4.32'],
])

function fail(workspace, message) {
  process.stderr.write(`brotli dependency contract (${workspace}): ${message}\n`)
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

function audit(workspace) {
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

audit('.')
audit('examples/app-demo')

if (process.exitCode === undefined) {
  process.stdout.write('brotli dependency contract: ok\n')
}
