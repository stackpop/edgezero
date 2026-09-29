#!/usr/bin/env bash
set -uo pipefail

scan() {
  local pattern=$1
  shift

  if matches="$(git grep -nE "$pattern" -- "$@")"; then
    printf '%s\n' "$matches"
    return 1
  fi

  local status=$?
  if [ "$status" -eq 1 ]; then
    return 0
  fi
  return "$status"
}

scan_detached_send_tuple() {
  node --input-type=module - "$@" <<'NODE'
import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'

const send = ['DetachedResponseEgressDecision::', 'Send'].join('')
const [decision, sendVariant] = send.split('::')

function skipRustTrivia(source, start) {
  let index = start

  while (index < source.length) {
    if (/\s/u.test(source[index])) {
      index += 1
      continue
    }
    if (source.startsWith('//', index)) {
      const newline = source.indexOf('\n', index + 2)
      index = newline === -1 ? source.length : newline + 1
      continue
    }
    if (source.startsWith('/*', index)) {
      let depth = 1
      index += 2
      while (index < source.length && depth > 0) {
        if (source.startsWith('/*', index)) {
          depth += 1
          index += 2
        } else if (source.startsWith('*/', index)) {
          depth -= 1
          index += 2
        } else {
          index += 1
        }
      }
      if (depth > 0) {
        return null
      }
      continue
    }
    break
  }

  return index
}

function findLegacyDetachedSendConstructors(source) {
  const matches = []
  let searchFrom = 0

  while (searchFrom < source.length) {
    const start = source.indexOf(decision, searchFrom)
    if (start === -1) {
      break
    }
    searchFrom = start + decision.length

    if (start > 0 && /[A-Za-z0-9_]/u.test(source[start - 1])) {
      continue
    }

    let index = skipRustTrivia(source, searchFrom)
    if (index === null || !source.startsWith('::', index)) {
      continue
    }
    index = skipRustTrivia(source, index + 2)
    if (index === null || !source.startsWith(sendVariant, index)) {
      continue
    }
    index = skipRustTrivia(source, index + sendVariant.length)
    if (index === null || source[index] !== '(') {
      continue
    }

    matches.push({
      index: start,
      text: source.slice(start, index + 1),
    })
  }

  return matches
}

let selfTestFailed = false
for (const [name, source] of [
  ['same-line tuple', `${send}(completion)`],
  ['newline tuple', `${send}\n(completion)`],
  ['line-comment tuple', `${send} // legacy\n(completion)`],
  ['block-comment tuple', `${send} /* legacy */ (completion)`],
  [
    'comment before variant',
    `${decision}:: /* legacy */ ${sendVariant}(completion)`,
  ],
  ['newline before path separator', `${decision}\n::${sendVariant}(completion)`],
  [
    'trivia around path separator',
    `${decision} /* before */ :: /* after */ ${sendVariant}(completion)`,
  ],
  [
    'nested block-comment trivia',
    `${decision} /* outer /* nested */ outer */ :: ` +
      `/* outer /* nested */ outer */ ${sendVariant} ` +
      '/* outer /* nested */ outer */ (completion)',
  ],
]) {
  if (findLegacyDetachedSendConstructors(source).length !== 1) {
    process.stderr.write(`legacy tuple checker self-test failed: ${name}\n`)
    selfTestFailed = true
  }
}
for (const source of [
  `${send} { completion, deadline }`,
  ['"DetachedResponseEgressDecision::",', '"Send("'].join('\n'),
]) {
  if (findLegacyDetachedSendConstructors(source).length !== 0) {
    process.stderr.write('legacy tuple checker self-test matched a supported form\n')
    selfTestFailed = true
  }
}
if (selfTestFailed) {
  process.exit(2)
}

const roots = process.argv.slice(2)
const docsIndexFixture = {
  path: 'docs/index.md',
  source: `${send} /* legacy */ (completion)`,
}
if (
  !roots.includes(docsIndexFixture.path) ||
  findLegacyDetachedSendConstructors(docsIndexFixture.source).length !== 1
) {
  process.stderr.write('legacy tuple checker did not reject the docs index fixture\n')
  process.exit(2)
}
const tracked = execFileSync('git', ['ls-files', '-z', '--', ...roots])
  .toString('utf8')
  .split('\0')
  .filter(Boolean)
let failed = false
for (const path of tracked) {
  const source = readFileSync(path, 'utf8')
  for (const match of findLegacyDetachedSendConstructors(source)) {
    const line = source.slice(0, match.index).split(/\r?\n/u).length
    process.stdout.write(`${path}:${line}:${match.text.replace(/\s+/gu, ' ')}\n`)
    failed = true
  }
}
process.exit(failed ? 1 : 0)
NODE
}

scan_platform_resource_legacy() {
  node --input-type=module - "$@" <<'NODE'
import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'

const memory = ['Memory', 'Ceiling'].join('')
const metadata = ['Platform', 'Metadata'].join('')
const retiredSource = [memory, 'Source'].join('')
const retiredAdapterMethod = ['Adapter::', 'memory_', 'ceiling'].join('')
const retiredClockConstructor = ['new_with_', 'monotonic_clock'].join('')
const retiredByValueAccessor =
  `pub const fn memory_${'ceiling'}(self) -> Option<${memory}>`
const allowlist = new Set([
  'CHANGELOG.md',
  'docs/superpowers/specs/2026-09-28-platform-resource-metadata-design.md',
])
const authoritativePlan =
  'docs/superpowers/plans/2026-09-28-platform-resource-metadata.md'

function maskRange(masked, source, start, end) {
  for (let index = start; index < end; index += 1) {
    if (source[index] !== '\n' && source[index] !== '\r') masked[index] = ' '
  }
}

function rawStringEnd(source, start) {
  let rawPrefix = start
  if (
    (source[start] === 'b' || source[start] === 'c') &&
    source[start + 1] === 'r'
  ) {
    rawPrefix += 1
  }
  if (source[rawPrefix] !== 'r') return null
  if (start > 0 && /[A-Za-z0-9_]/u.test(source[start - 1])) return null

  let quote = rawPrefix + 1
  while (source[quote] === '#') quote += 1
  if (source[quote] !== '"') return null
  const hashes = source.slice(rawPrefix + 1, quote)
  const closing = `"${hashes}`
  const closingStart = source.indexOf(closing, quote + 1)
  return closingStart === -1 ? source.length : closingStart + closing.length
}

function characterLiteralEnd(source, start) {
  if (source[start] !== "'") return null
  let index = start + 1
  if (source[index] === '\\') {
    index += 1
    if (source[index] === 'u' && source[index + 1] === '{') {
      const brace = source.indexOf('}', index + 2)
      if (brace === -1) return null
      index = brace + 1
    } else {
      index += 1
    }
  } else {
    const codePoint = source.codePointAt(index)
    if (codePoint === undefined || source[index] === '\n' || source[index] === '\r') {
      return null
    }
    index += codePoint > 0xffff ? 2 : 1
  }
  return source[index] === "'" ? index + 1 : null
}

function maskRustCommentsAndLiterals(source) {
  const masked = source.split('')
  let index = 0
  while (index < source.length) {
    if (source.startsWith('//', index)) {
      const newline = source.indexOf('\n', index + 2)
      const end = newline === -1 ? source.length : newline
      maskRange(masked, source, index, end)
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
      maskRange(masked, source, index, end)
      index = end
      continue
    }
    const rawEnd = rawStringEnd(source, index)
    if (rawEnd !== null) {
      maskRange(masked, source, index, rawEnd)
      index = rawEnd
      continue
    }
    if (source[index] === '"') {
      let end = index + 1
      while (end < source.length) {
        if (source[end] === '\\') {
          end += 2
        } else if (source[end] === '"') {
          end += 1
          break
        } else {
          end += 1
        }
      }
      maskRange(masked, source, index, end)
      index = end
      continue
    }
    const characterEnd = characterLiteralEnd(source, index)
    if (characterEnd !== null) {
      maskRange(masked, source, index, characterEnd)
      index = characterEnd
      continue
    }
    index += 1
  }
  return masked.join('')
}

function closingDelimiter(source, open) {
  const pairs = new Map([
    ['(', ')'],
    ['[', ']'],
    ['{', '}'],
  ])
  const stack = [pairs.get(source[open])]
  for (let index = open + 1; index < source.length; index += 1) {
    if (source.startsWith('//', index)) {
      const newline = source.indexOf('\n', index + 2)
      index = newline === -1 ? source.length : newline
    } else if (source.startsWith('/*', index)) {
      let depth = 1
      index += 2
      while (index < source.length && depth > 0) {
        if (source.startsWith('/*', index)) {
          depth += 1
          index += 2
        } else if (source.startsWith('*/', index)) {
          depth -= 1
          index += 2
        } else {
          index += 1
        }
      }
      index -= 1
    } else {
      const rawEnd = rawStringEnd(source, index)
      if (rawEnd !== null) {
        index = rawEnd - 1
        continue
      }
      if (source[index] === '"') {
        index += 1
        while (index < source.length && source[index] !== '"') {
          if (source[index] === '\\') index += 1
          index += 1
        }
        continue
      }
      const characterEnd = characterLiteralEnd(source, index)
      if (characterEnd !== null) {
        index = characterEnd - 1
        continue
      }
      if (pairs.has(source[index])) {
        stack.push(pairs.get(source[index]))
      } else if (source[index] === stack.at(-1)) {
        stack.pop()
        if (stack.length === 0) return index
      }
    }
  }
  return -1
}

function closingGenericList(source, open) {
  let angleDepth = 1
  const delimiters = []
  const pairs = new Map([
    ['(', ')'],
    ['[', ']'],
    ['{', '}'],
  ])
  for (let index = open + 1; index < source.length; index += 1) {
    if (pairs.has(source[index])) {
      delimiters.push(pairs.get(source[index]))
      continue
    }
    if (source[index] === delimiters.at(-1)) {
      delimiters.pop()
      continue
    }
    if (delimiters.length !== 0) continue
    if (source[index] === '<') angleDepth += 1
    else if (source[index] === '>' && source[index - 1] !== '-') {
      angleDepth -= 1
      if (angleDepth === 0) return index
    }
  }
  return -1
}

function previousNonWhitespace(source, start, count) {
  const characters = []
  for (let index = start - 1; index >= 0 && characters.length < count; index -= 1) {
    if (!/\s/u.test(source[index])) characters.unshift(source[index])
  }
  return characters.join('')
}

function closureParametersStart(source, argumentStart, pipe) {
  const prefix = source.slice(argumentStart, pipe).trim()
  return /^(?:(?:&(?:\s*mut)?|[*!-])\s*)*(?:async\s+)?(?:move\s+)?$/u.test(prefix)
}

function topLevelArguments(source, open, close) {
  const argumentsList = []
  let start = open + 1
  const delimiters = []
  let angleDepth = 0
  let closureParameters = false

  for (let index = start; index <= close; index += 1) {
    if (source.startsWith('//', index)) {
      const newline = source.indexOf('\n', index + 2)
      index = newline === -1 ? close : Math.min(newline, close)
    } else if (source.startsWith('/*', index)) {
      let depth = 1
      index += 2
      while (index < close && depth > 0) {
        if (source.startsWith('/*', index)) {
          depth += 1
          index += 2
        } else if (source.startsWith('*/', index)) {
          depth -= 1
          index += 2
        } else {
          index += 1
        }
      }
      index -= 1
      continue
    }

    const rawEnd = rawStringEnd(source, index)
    if (rawEnd !== null) {
      index = rawEnd - 1
      continue
    }
    if (source[index] === '"') {
      index += 1
      while (index < close && source[index] !== '"') {
        if (source[index] === '\\') index += 1
        index += 1
      }
      continue
    }
    const characterEnd = characterLiteralEnd(source, index)
    if (characterEnd !== null) {
      index = characterEnd - 1
      continue
    }

    if (closureParameters) {
      if (source[index] === '|') closureParameters = false
      continue
    }
    if (
      delimiters.length === 0 &&
      angleDepth === 0 &&
      source[index] === '|' &&
      closureParametersStart(source, start, index)
    ) {
      if (source[index + 1] === '|') index += 1
      else closureParameters = true
      continue
    }

    if (source[index] === '<' && (angleDepth > 0 || previousNonWhitespace(source, index, 2) === '::')) {
      angleDepth += 1
      continue
    }
    if (source[index] === '>' && angleDepth > 0) {
      angleDepth -= 1
      continue
    }
    if (angleDepth > 0) continue

    if ('([{'.includes(source[index])) {
      delimiters.push(new Map([
        ['(', ')'],
        ['[', ']'],
        ['{', '}'],
      ]).get(source[index]))
      continue
    }
    if (source[index] === delimiters.at(-1)) {
      delimiters.pop()
      continue
    }
    if (index === close || (source[index] === ',' && delimiters.length === 0)) {
      const argument = source.slice(start, index).trim()
      if (argument !== '') argumentsList.push(argument)
      start = index + 1
    }
  }
  return argumentsList
}

function receiverKind(receiver) {
  if (/^(?:mut\s+)?self(?:\s*:|$)/u.test(receiver)) return 'owned'
  if (/^&\s*(?:'[A-Za-z_][A-Za-z0-9_]*\s+)?(?:mut\s+)?self(?:\s*:|$)/u.test(receiver)) {
    return 'borrowed'
  }
  return null
}

function retiredMemoryAccessorDeclarations(source, originalSource) {
  const matches = []
  const declaration = /\bfn\s+memory_ceiling\b/gu
  for (const match of source.matchAll(declaration)) {
    let open = match.index + match[0].length
    while (/\s/u.test(source[open] ?? '')) open += 1
    if (source[open] === '<') {
      const genericClose = closingGenericList(source, open)
      if (genericClose === -1) continue
      open = genericClose + 1
      while (/\s/u.test(source[open] ?? '')) open += 1
    }
    if (source[open] !== '(') continue
    const close = closingDelimiter(source, open)
    if (close === -1) continue
    const argumentsList = topLevelArguments(source, open, close)
    if (argumentsList.length !== 1) continue
    const receiver = receiverKind(argumentsList[0])
    if (receiver === null) continue

    const returnType = source.slice(close + 1).match(/^\s*->\s*((?:(?:std|core)\s*::\s*option\s*::\s*)?Option)\b/u)
    if (returnType === null) continue
    const end = close + 1 + returnType.index + returnType[0].length
    matches.push({ index: match.index, text: originalSource.slice(match.index, end) })
  }
  return matches
}

function constructorMatches(source, originalSource, typeName, argumentCount) {
  const constructor = new RegExp(
    `\\b${typeName}\\b\\s*::\\s*new\\s*\\(`,
    'gu',
  )
  const matches = []
  for (const match of source.matchAll(constructor)) {
    const start = match.index
    const open = start + match[0].lastIndexOf('(')
    const close = closingDelimiter(source, open)
    if (close === -1) continue
    if (topLevelArguments(source, open, close).length === argumentCount) {
      matches.push({ index: start, text: originalSource.slice(start, close + 1) })
    }
  }
  return matches
}

function finalNamedCutoffArguments(source, originalSource) {
  const matches = []
  const call = /\b(?:start_batch_until|send_all_until)\s*\(/gu
  for (const match of source.matchAll(call)) {
    const open = match.index + match[0].lastIndexOf('(')
    const close = closingDelimiter(source, open)
    if (close === -1) continue
    const finalArgument = topLevelArguments(source, open, close).at(-1)
    if (/^(?:cutoff|_cutoff|batch_cutoff)$/u.test(finalArgument ?? '')) {
      matches.push({
        index: match.index,
        text: originalSource.slice(match.index, close + 1),
      })
    }
  }
  return matches
}

function regexMatches(source, pattern, originalSource) {
  return [...source.matchAll(pattern)].map((match) => ({
    index: match.index,
    text: originalSource.slice(match.index, match.index + match[0].length),
  }))
}

function findLegacyForms(source, rustSource = false, originalSource = source) {
  const searchable = rustSource ? maskRustCommentsAndLiterals(source) : source
  const memoryAccessor = ['memory_', 'ceiling'].join('')
  return [
    ...regexMatches(
      searchable,
      new RegExp(`\\b${retiredSource}\\b`, 'gu'),
      originalSource,
    ),
    ...regexMatches(
      searchable,
      new RegExp(`\\b${retiredAdapterMethod.replace('::', '\\s*::\\s*')}\\b`, 'gu'),
      originalSource,
    ),
    ...retiredMemoryAccessorDeclarations(searchable, originalSource),
    ...regexMatches(
      searchable,
      new RegExp(`\\b${retiredClockConstructor}\\b`, 'gu'),
      originalSource,
    ),
    ...regexMatches(
      searchable,
      new RegExp(
        `(?:${metadata}\\s*::\\s*|\\.)${memoryAccessor}\\s*\\(\\s*\\)\\s*->\\s*Option\\b`,
        'gu',
      ),
      originalSource,
    ),
    ...regexMatches(
      searchable,
      /\b(?:cutoff|_cutoff|batch_cutoff)\s*:\s*(?:Option\s*<\s*)?(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*Deadline\s*>?/gu,
      originalSource,
    ),
    ...regexMatches(
      searchable,
      /\blet\s+(?:mut\s+)?(?:cutoff|_cutoff|batch_cutoff)\b/gu,
      originalSource,
    ),
    ...constructorMatches(searchable, originalSource, metadata, 1),
    ...constructorMatches(searchable, originalSource, memory, 4),
    ...finalNamedCutoffArguments(searchable, originalSource),
  ]
}

function isRustSource(path) {
  return path.endsWith('.rs') || path.endsWith('.rs.hbs')
}

function isMarkdownSource(path) {
  return path.endsWith('.md') || path.endsWith('.mdx')
}

function fenceOpening(line) {
  const match = /^ {0,3}(`{3,}|~{3,})[ \t]*([^\r\n]*)$/u.exec(line)
  if (match === null) return null
  if (match[1][0] === '`' && match[2].includes('`')) return null
  const language = match[2].trim().split(/[:,\s]/u, 1)[0].toLowerCase()
  return {
    marker: match[1][0],
    minimumLength: match[1].length,
    rust: language === 'rust' || language === 'rs',
  }
}

function isFenceClosing(line, fence) {
  let index = 0
  while (index < 3 && line[index] === ' ') index += 1
  let markerLength = 0
  while (line[index + markerLength] === fence.marker) markerLength += 1
  return (
    markerLength >= fence.minimumLength &&
    line.slice(index + markerLength).trim() === ''
  )
}

function copyRange(view, source, start, end) {
  for (let index = start; index < end; index += 1) view[index] = source[index]
}

function markdownViews(source) {
  const prose = source.split('')
  const rust = source.split('')
  maskRange(prose, source, 0, source.length)
  maskRange(rust, source, 0, source.length)
  let fence = null
  let lineStart = 0

  while (lineStart < source.length) {
    const newline = source.indexOf('\n', lineStart)
    const lineEnd = newline === -1 ? source.length : newline
    const nextLine = newline === -1 ? source.length : newline + 1
    const line = source.slice(lineStart, lineEnd).replace(/\r$/u, '')

    if (fence === null) {
      const opening = fenceOpening(line)
      if (opening !== null) {
        fence = {
          ...opening,
          contentStart: newline === -1 ? source.length : newline + 1,
        }
      } else {
        copyRange(prose, source, lineStart, nextLine)
      }
    } else if (isFenceClosing(line, fence)) {
      if (fence.rust) copyRange(rust, source, fence.contentStart, lineStart)
      fence = null
    }

    if (newline === -1) break
    lineStart = nextLine
  }

  if (fence?.rust) copyRange(rust, source, fence.contentStart, source.length)
  return { prose: prose.join(''), rust: rust.join('') }
}

function deduplicateMatches(matches) {
  const unique = new Map()
  for (const match of matches) {
    unique.set(`${match.index}:${match.text}`, match)
  }
  return [...unique.values()].sort(
    (left, right) => left.index - right.index || left.text.localeCompare(right.text),
  )
}

function findLegacyFormsForPath(path, source) {
  if (isRustSource(path)) return findLegacyForms(source, true)
  if (!isMarkdownSource(path)) return findLegacyForms(source)
  const views = markdownViews(source)
  const proseMatches = findLegacyForms(views.prose, false, source)
  const rustFenceMatches = findLegacyForms(views.rust, true, source)
  return deduplicateMatches([...proseMatches, ...rustFenceMatches])
}

const bareCutoff = ['cut', 'off'].join('')
const batchCutoff = `batch_${bareCutoff}`
const fixtures = [
  retiredSource,
  `pub type ${retiredSource} = PlatformResourceSource;`,
  `pub /* compatibility */ type /* name */ ${retiredSource} = PlatformResourceSource;`,
  retiredAdapterMethod,
  `Adapter /* path */ :: /* method */ memory_${'ceiling'}`,
  `fn /* trait */ memory_${'ceiling'} /* call */ ( /* receiver */ & /* borrow */ self /* end */ ) -> Option<${memory}> {}`,
  retiredByValueAccessor,
  `pub(crate) unsafe fn memory_${'ceiling'}(mut self) -> Option<${memory}>`,
  `pub async fn memory_${'ceiling'}(&mut self) -> Option<${memory}>`,
  `pub fn memory_${'ceiling'}<'a>(&'a self) -> Option<${memory}>`,
  `pub fn memory_${'ceiling'}<F: Fn() -> Option<T>>(&self) -> Option<${memory}>`,
  `${metadata}::new(memory)`,
  `${metadata} /* type */ :: /* path */ new /* call */ (memory /*, ignored */)`,
  `${metadata}::new(make::<'a, A, B>(memory))`,
  `${metadata}::new(make::<A, B>())`,
  `${metadata}::new(|left, right| combine(left, right))`,
  `${metadata}::new(&|left, right| combine(left, right))`,
  `${metadata}::new(&mut |left, right| combine(left, right))`,
  `${memory}::new(total, scope, stack, source)`,
  `${memory} /* type */ :: /* path */ new /* call */ (total, scope /*, ignored */, stack, source)`,
  `${memory}::new(make::<A, B>(), scope, stack, source)`,
  `${memory}::new(total, |left, right| scope(left, right), stack, source)`,
  `${memory}::new(total, &|left, right| scope(left, right), stack, source)`,
  retiredClockConstructor,
  `${bareCutoff}: Deadline`,
  `let ${batchCutoff} = deadline();`,
  `client.send_all_until(requests, ${bareCutoff})`,
]
const supported = [
  `${metadata}::new(memory, population, accounting)`,
  `${metadata}::new(memory, population /*, ignored */, accounting)`,
  `${metadata}::new(make::<A, B>(), population, |left, right| accounting(left, right))`,
  `${metadata}::new(make::<A, B>(), population, &|left, right| accounting(left, right))`,
  `${memory}::new(total, scope, stack)`,
  `${memory}::new(total, scope /*, ignored */, stack)`,
  `${memory}::new(make::<'a, A, B>(), |left, right| scope(left, right), stack)`,
  `pub const fn memory_${'ceiling'}(self) -> PlatformFact<${memory}>`,
  `pub const fn memory_${'ceiling'}(&self) -> PlatformFact<${memory}>`,
  `pub fn memory_${'ceiling'}<F: Fn() -> Option<T>>(&self) -> PlatformFact<${memory}>`,
  'fn platform_metadata(&self) -> PlatformMetadata {}',
  'observation_cutoff: Deadline',
  'let observation_cutoff = deadline();',
  'client.send_all_until(requests, observation_cutoff)',
  `// ${retiredSource}\nconst VALUE: usize = 1;`,
  `/* ${retiredAdapterMethod} */ const VALUE: usize = 1;`,
  `const TEXT: &str = "${metadata}::new(memory)";`,
  `const RAW: &str = r#"fn memory_${'ceiling'}(&self)"#;`,
  `const RAW_C: &CStr = cr#"quoted " ${retiredSource}"#;`,
  `const BYTE: u8 = b'${bareCutoff.charAt(0)}';`,
]

for (const fixture of fixtures) {
  if (findLegacyForms(fixture, true).length === 0) {
    process.stderr.write(`platform metadata checker missed fixture: ${fixture}\n`)
    process.exit(2)
  }
}
for (const fixture of supported) {
  if (findLegacyForms(fixture, true).length !== 0) {
    process.stderr.write(`platform metadata checker rejected supported fixture: ${fixture}\n`)
    process.exit(2)
  }
}
for (const fixture of [
  `Prose names ${retiredSource}.`,
  `\`${metadata}::new(memory)\``,
  `<!-- ${retiredAdapterMethod} -->`,
]) {
  if (findLegacyFormsForPath('docs/guide/legacy-fixture.md', fixture).length === 0) {
    process.stderr.write('platform metadata checker masked a Markdown fixture\n')
    process.exit(2)
  }
}
for (const [name, fixture, expectedStart] of [
  [
    'plain Rust fence',
    `before\n\`\`\`rust\n${metadata} /* trivia */ :: new(memory)\n\`\`\`\nafter`,
    metadata,
  ],
  [
    'modified Rust fence',
    `\`\`\`rust,no_run\nAdapter /* path */ :: /* method */ memory_${'ceiling'}\n\`\`\``,
    'Adapter',
  ],
  [
    'modified rs fence',
    `\`\`\`rs,ignore\nfn /* trait */ memory_${'ceiling'} /* call */ ( & /* receiver */ self ) -> Option<${memory}> {}\n\`\`\``,
    'fn',
  ],
  [
    'tilde Rust fence with colon modifier',
    `~~~rust:no_run\n${metadata} /* trivia */ :: new(memory)\n~~~~`,
    metadata,
  ],
  [
    'indented Rust fence with space modifier',
    `   \`\`\`rust no_run\nAdapter /* path */ :: memory_${'ceiling'}\n   \`\`\``,
    'Adapter',
  ],
  [
    'variable-length rs fence',
    `\`\`\`\`rs:ignore\n${metadata} /* trivia */ :: new(memory)\n\`\`\`\`\``,
    metadata,
  ],
]) {
  const matches = findLegacyFormsForPath('docs/guide/legacy-fixture.md', fixture)
  if (matches.length !== 1 || matches[0].index !== fixture.indexOf(expectedStart)) {
    process.stderr.write(`platform metadata checker missed ${name}\n`)
    process.exit(2)
  }
}
const nonRustFence =
  `\`\`\`text\n${metadata} /* trivia */ :: new(memory)\n\`\`\``
if (findLegacyFormsForPath('docs/guide/legacy-fixture.md', nonRustFence).length !== 0) {
  process.stderr.write('platform metadata checker parsed a non-Rust fence as Rust\n')
  process.exit(2)
}
const nonRustRawHistory = `\`\`\`text\n${retiredSource}\n\`\`\``
if (findLegacyFormsForPath('docs/guide/legacy-fixture.md', nonRustRawHistory).length !== 0) {
  process.stderr.write('platform metadata checker scanned a non-Rust fence\n')
  process.exit(2)
}
const invalidBacktickFence =
  `\`\`\`rust\`bad\n${metadata}::new(memory)\n\`\`\``
if (findLegacyFormsForPath('docs/guide/legacy-fixture.md', invalidBacktickFence).length === 0) {
  process.stderr.write('platform metadata checker treated an invalid backtick fence as a fence\n')
  process.exit(2)
}
const rustNonCodeFixture =
  `\`\`\`rust\n// ${retiredSource}\n` +
  `const TEXT: &str = "${retiredByValueAccessor}";\n` +
  `const RAW: &str = r#"${retiredAdapterMethod}"#;\n\`\`\``
if (
  findLegacyFormsForPath('docs/guide/legacy-fixture.md', rustNonCodeFixture).length !== 0
) {
  process.stderr.write('platform metadata checker scanned Rust fence non-code\n')
  process.exit(2)
}
const duplicateFixture = `\`\`\`rust\n${metadata}::new(memory)\n\`\`\``
if (findLegacyFormsForPath('docs/guide/legacy-fixture.md', duplicateFixture).length !== 1) {
  process.stderr.write('platform metadata checker did not deduplicate Markdown matches\n')
  process.exit(2)
}

const roots = process.argv.slice(2)
const isAllowlisted = (path) => allowlist.has(path)
const rootCovers = (root, path) => path === root || path.startsWith(`${root}/`)
const requiredRootError = (candidateRoots) =>
  candidateRoots.some((root) => rootCovers(root, authoritativePlan))
    ? null
    : `platform metadata checker does not scan ${authoritativePlan}`
const rootsWithPlan = [...roots, authoritativePlan]
const rootsWithoutPlan = rootsWithPlan.filter(
  (root) => !rootCovers(root, authoritativePlan),
)
if (
  requiredRootError(rootsWithPlan) !== null ||
  requiredRootError(rootsWithoutPlan) === null
) {
  process.stderr.write('platform metadata checker plan-root self-test failed\n')
  process.exit(2)
}
const rootError = requiredRootError(roots)
if (rootError !== null) {
  process.stderr.write(`${rootError}\n`)
  process.exit(2)
}
for (const root of roots) {
  const fixturePath = root.includes('.') ? root : `${root}/legacy-fixture.rs`
  if (isAllowlisted(fixturePath)) continue
  if (findLegacyFormsForPath(fixturePath, retiredSource).length === 0) {
    process.stderr.write(
      `platform metadata checker did not reject a fixture in ${fixturePath}\n`,
    )
    process.exit(2)
  }
}
for (const path of allowlist) {
  const nearMiss = `${path}.active`
  if (
    !isAllowlisted(path) ||
    isAllowlisted(nearMiss) ||
    findLegacyForms(fixtures.join('\n')).length === 0
  ) {
    process.stderr.write(`platform metadata checker allowlist self-test failed for ${path}\n`)
    process.exit(2)
  }
}
if (allowlist.size !== 2) {
  process.stderr.write('platform metadata checker allowlist must contain exactly two paths\n')
  process.exit(2)
}

const tracked = execFileSync('git', [
  'ls-files',
  '--cached',
  '--others',
  '--exclude-standard',
  '-z',
  '--',
  ...roots,
])
  .toString('utf8')
  .split('\0')
  .filter(Boolean)
let failed = false
for (const path of tracked) {
  if (isAllowlisted(path)) continue
  const source = readFileSync(path, 'utf8')
  for (const match of findLegacyFormsForPath(path, source)) {
    const line = source.slice(0, match.index).split(/\r?\n/u).length
    process.stdout.write(`${path}:${line}:${match.text.replace(/\s+/gu, ' ')}\n`)
    failed = true
  }
}
process.exit(failed ? 1 : 0)
NODE
}

active_surfaces=(
  crates
  examples/app-demo
  .github/actions
  .github/workflows
  docs/.vitepress
  docs/guide
  docs/index.md
  docs/specs
  docs/superpowers/specs
  docs/superpowers/plans/2026-09-26-platform-memory-ceiling.md
  docs/superpowers/plans/2026-09-28-platform-resource-metadata.md
  scripts
  README.md
  CLAUDE.md
  TODO.md
  Cargo.toml
  CHANGELOG.md
)

assert_active_surface_fixture_rejected() {
  local path=$1
  local pattern=$2
  local source=$3
  local active

  for active in "${active_surfaces[@]}"; do
    if [ "$active" = "$path" ]; then
      if printf '%s\n' "$source" | grep -Eq "$pattern"; then
        return 0
      fi
      printf 'legacy checker did not reject the %s fixture\n' "$path" >&2
      return 2
    fi
  done

  printf 'legacy checker does not scan the %s fixture\n' "$path" >&2
  return 2
}

failed=0

scan \
  'Proxy(Client|Handle|Request|Response|Service)|proxy_handle|edgezero_core::proxy|crate::proxy|pub mod proxy' \
  crates examples/app-demo docs/guide README.md CLAUDE.md TODO.md Cargo.toml \
  .claude/agents/code-architect.md || failed=1

# Downstream response delivery is now adapter-owned. Keep the outbound-fetch
# buffering helpers: they implement ResponseMode::Buffered and are unrelated to
# response egress.
scan \
  'ResponseReturned|begin_owned|[A-Z]+_RESPONSE_STREAM_BUFFER_BYTES|SpinFullResponse|Captured(Cloudflare|Spin)Response|EdgeZeroAxumService|dispatch_for_test|pub (async )?fn (from_core_response|into_axum_response)|buffered (fallback|passthrough)|16 MiB downstream conversion fallback|Response -> Workers Response \(buffered bodies\)' \
  crates examples/app-demo docs/guide README.md TODO.md Cargo.toml \
  ':(exclude)crates/edgezero-cli/src/generator.rs' || failed=1

# The generator keeps a negative assertion containing this spelling. Scan the
# rendered source inputs, demos, and user-facing guides where an attribute would
# preserve the retired Fastly response-returning entrypoint.
scan \
  '#\[fastly::main\]' \
  crates/edgezero-adapter-fastly/src/templates examples/app-demo docs/guide README.md || failed=1

# The completion-driven hard cut removes the terminal-vector-only API and the
# request-extension-only Fastly lifecycle. The generator's negative assertion
# intentionally contains `.send_all(` and is excluded here.
scan \
  '\.send_all\(|SendAllSlotIsolation|send-all-slot-isolation|run_app_with_request_extensions' \
  crates examples/app-demo docs/guide README.md CLAUDE.md TODO.md \
  ':(exclude)crates/edgezero-cli/src/generator.rs' || failed=1

# Batch termination is explicit. Adapter drivers cannot use stream EOF as a
# cutoff signal, and callers cannot interpret every missing item as timeout.
scan \
  'OutboundBatch::from_stream|while[[:space:]]+let[[:space:]]+Some\([^)]*\)[[:space:]]*=[[:space:]]*batch\.next\(\)\.await|send_all_until\(.*\)\.await\.slots|Result<OutboundBatchResults,[[:space:]]*EdgeError>' \
  crates examples/app-demo docs/guide README.md CLAUDE.md TODO.md || failed=1

# Generated/demo Fastly entrypoints must delegate transmission to EdgeZero.
scan \
  'stream_to_client|send_to_client|send_with_registries' \
  crates/edgezero-adapter-fastly/src/templates examples/app-demo/crates/app-demo-adapter-fastly || failed=1

# Application assembly and ingress response ownership are hard-cut contracts.
# Configuration must be fallible, every response-producing admission variant
# uses named fields, and response completion never travels through HTTP
# extensions or an envelope escape hatch.
scan \
  'fn configure(_app)?\([^)]*&mut (EdgeZeroApp|edgezero_core::app::App|App)[^)]*\)[[:space:]]*\{' \
  crates examples/app-demo docs/guide README.md || failed=1

scan \
  'fn build_app\(\)[[:space:]]*->[[:space:]]*App|AdmissionDecision::Refuse[[:space:]]*\(|ResponseEgressEnvelope[^[:space:]]*\.into_response|envelope\.into_response\(\)' \
  crates examples/app-demo docs/guide README.md || failed=1

scan \
  '(extensions(_mut)?\(\)|extensions)\.insert\(ResponseEgressCompletion' \
  crates examples/app-demo docs/guide README.md \
  ':(exclude)crates/edgezero-core/src/response_egress.rs' || failed=1

# Normalized ingress errors use one typed send-or-abort decision. The generator
# keeps the retired setter spelling only in a negative assertion.
scan \
  'DetachedResponseEgressCompletionFactory|set_detached_response_egress_completion_factory' \
  crates examples/app-demo docs/guide docs/superpowers/specs README.md \
  ':(exclude)crates/edgezero-cli/src/generator.rs' || failed=1

# Detached response egress uses named fields only. This scanner spans comments
# and newlines, and its embedded fixtures keep those cases covered in CI.
scan_detached_send_tuple "${active_surfaces[@]}" || failed=1

# Platform resource facts hard-cut the old memory-only trait and constructors.
# Only the migration record and focused design retain historical signatures.
scan_platform_resource_legacy "${active_surfaces[@]}" || failed=1

# Response-owned deadlines use explicit absolute or egress-start-relative constructors. Keep the
# removed ambiguous constructor and unresolved accessor out of active code and documentation.
deadline_legacy_pattern='ResponseEgressDeadline::ne'
deadline_legacy_pattern+='w[[:space:]]*\(|ResponseEgressDeadline::dead'
deadline_legacy_pattern+='line|pub[[:space:]]+(const[[:space:]]+)?fn[[:space:]]+dead'
deadline_legacy_pattern+='line\(self\)'
deadline_legacy_fixture='ResponseEgressDeadline::ne'
deadline_legacy_fixture+='w(deadline)'
assert_active_surface_fixture_rejected \
  docs/index.md "$deadline_legacy_pattern" "$deadline_legacy_fixture" || failed=1
scan "$deadline_legacy_pattern" "${active_surfaces[@]}" || failed=1

# Retired staging command identifiers must not return to active code or public
# documentation. Historical plans are outside these paths, and the exact terms
# below intentionally do not match ordinary Fastly "staged" provider-state prose.
staging_pattern='DeploySta'
staging_pattern+='ged|deploy_sta'
staging_pattern+='ged|deploy-sta'
staging_pattern+='ged|--sta'
staging_pattern+='ged'
staging_fixture='deploy-sta'
staging_fixture+='ged'
assert_active_surface_fixture_rejected \
  docs/index.md "$staging_pattern" "$staging_fixture" || failed=1
scan "$staging_pattern" "${active_surfaces[@]}" || failed=1

exit "$failed"
