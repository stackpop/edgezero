import assert from "node:assert/strict";
import test from "node:test";
import { execFileSync, spawnSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  checkTransition,
  parseRecord,
  scanDocument,
  selectRange,
  verifyRelease,
} from "./check-doc-action-pins.mjs";

const revision = "1".repeat(40);
const record = {
  "action-revision": revision,
  "action-version": "v1.2.3",
  "schema-version": 1,
};
const bytes = JSON.stringify(record);
const path = "docs/guide/deploy-github-actions.md";
const example = (ref = "<EDGEZERO_ACTION_VERSION>", label = "ubuntu-24.04") =>
  `jobs:\n  deploy:\n    runs-on: ${label}\n    steps:\n      - uses: stackpop/edgezero/.github/actions/deploy-fastly@${ref}\n`;
const fence = (yaml) => `\n~~~yaml\n${yaml}~~~\n`;

test("record requires exact canonical bytes, fields, and values", () => {
  assert.deepEqual(parseRecord(Buffer.from(bytes)), record);
  for (const value of [
    bytes + "\n",
    " " + bytes,
    bytes.replace('"schema-version":1', '"schema-version":1.0'),
    bytes.replace(
      '"schema-version":1',
      '"schema-version":1,"schema-version":1',
    ),
    JSON.stringify({ ...record, extra: 1 }),
    JSON.stringify({ ...record, "action-version": "v01.2.3" }),
    JSON.stringify({ ...record, "action-revision": "0".repeat(40) }),
  ]) {
    assert.throws(() => parseRecord(Buffer.from(value)));
  }
});

test("fences use the Markdown AST including list and quote nesting", () => {
  assert.equal(scanDocument(path, fence(example()), null), 1);
  const nested =
    "- Example\n\n" +
    fence(example())
      .split("\n")
      .map((line) => `  ${line}`)
      .join("\n");
  assert.equal(scanDocument(path, nested, null), 1);
  const quoted = fence(example())
    .split("\n")
    .map((line) => `> ${line}`)
    .join("\n");
  assert.equal(scanDocument(path, quoted, null), 1);
  assert.throws(() => scanDocument("README.md", quoted, null));
  assert.throws(() => scanDocument(path, fence(example("v1")), null));
  assert.throws(() =>
    scanDocument(path, fence(example("v1.2.3", "ubuntu-latest")), record),
  );
  assert.throws(() => scanDocument(path, fence(example("v1.2.4")), record));
  assert.equal(scanDocument(path, fence(example("v1.2.3")), record), 1);
});

test("job-level reusable callers omit steps and runner selection", () => {
  const call =
    "jobs:\n  build:\n    uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@v1.2.3\n";
  assert.equal(scanDocument(path, fence(call), record), 1);
  assert.throws(() =>
    scanDocument(path, fence(call + "    runs-on: ubuntu-24.04\n"), record),
  );
  assert.throws(() =>
    scanDocument(path, fence(call + "    steps: []\n"), record),
  );
});

test("annotated YAML fences and root-repository refs cannot bypass policy", () => {
  for (const language of [
    "yaml{1}",
    "yml:line-numbers",
    "yaml [workflow.yml]",
    "yaml{1} [workflow.yml]",
  ]) {
    const valid = `\n\`\`\`${language}\n${example("v1.2.3")}\`\`\`\n`;
    assert.equal(scanDocument(path, valid, record), 1);
    assert.throws(() =>
      scanDocument(path, valid.replace("@v1.2.3", "@main"), record),
    );
  }
  for (const action of [
    "stackpop/edgezero",
    "StackPop/EdgeZero",
    "StackPop/EdgeZero/.github/actions/deploy-fastly",
  ]) {
    assert.throws(() =>
      scanDocument(path, fence(`steps:\n  - uses: ${action}@v1.2.3\n`), record),
    );
  }
  assert.equal(
    scanDocument(
      path,
      "The prepublication placeholder was `<EDGEZERO_ACTION_VERSION>`.\n" +
        fence(example("v1.2.3")),
      record,
    ),
    1,
  );
  assert.throws(() => scanDocument(path, fence(example()), record));
});

test("fragments cannot hide action refs, nulls, or mixed versions", () => {
  for (const yaml of [
    "steps:\n  - uses: actions/checkout@v7\n",
    "- uses: actions/checkout@null\n",
    "uses: actions/checkout@v7\n",
    "steps:\n  - uses: null\n",
    "steps:\n  - uses: |\n      actions/checkout@v7.0.1\n",
    "steps:\n  - uses: stackpop/edgezero/.github/actions/deploy-fastly@v1.2.3\n",
    example("v1.2.3") +
      "      - uses: StackPop/EdgeZero/.github/actions/deploy-fastly@v1.2.4\n",
  ])
    assert.throws(() => scanDocument(path, fence(yaml), record));
  assert.equal(
    scanDocument(
      "README.md",
      fence("steps:\n  - uses: actions/checkout@v7.0.1\n"),
      null,
    ),
    1,
  );
});

test("decoded placeholders and mismatched invocation kinds fail", () => {
  const hidden = 'env:\n  EXAMPLE: "\\u003cEDGEZERO_ACTION_VERSION>"\n';
  assert.throws(() =>
    scanDocument(path, fence(hidden + example("v1.2.3")), record),
  );
  assert.throws(() =>
    scanDocument(
      path,
      fence(
        "jobs:\n  build:\n    uses: stackpop/edgezero/.github/actions/deploy-fastly@v1.2.3\n",
      ),
      record,
    ),
  );
  assert.throws(() =>
    scanDocument(
      path,
      fence(
        example("v1.2.3").replace(
          ".github/actions/deploy-fastly",
          ".github/workflows/build-app-cli.yml",
        ),
      ),
      record,
    ),
  );
});

test("workflow filters include every structural gate input family", () => {
  const workflow = fileURLToPath(
    new URL("../../../workflows/deploy-action.yml", import.meta.url),
  );
  const document = JSON.parse(
    execFileSync("yq", ["-o=json", ".", workflow], { encoding: "utf8" }),
  );
  for (const event of ["pull_request", "push"]) {
    const filters = document.on[event].paths;
    for (const required of [
      "**/action.yml",
      "**/action.yaml",
      "docs/.edgezero-action-release.json",
      "**/*.[mM][dD]",
      "**/*.[mM][aA][rR][kK][dD][oO][wW][nN]",
    ]) {
      assert.ok(filters.includes(required), `${event} omits ${required}`);
    }
  }
});

test("release transitions are one-way, atomic, and independently verified", () => {
  const calls = [];
  const verify = (value) => calls.push(value);
  checkTransition(null, null, ["code.rs"], verify);
  checkTransition(
    null,
    record,
    ["docs/a.md", "docs/.edgezero-action-release.json"],
    verify,
  );
  assert.deepEqual(calls, [record]);
  checkTransition(record, record, ["code.rs"], verify);
  assert.equal(calls.length, 1);
  const next = { ...record, "action-version": "v1.2.4" };
  checkTransition(
    record,
    next,
    ["docs/a.md", "docs/.edgezero-action-release.json"],
    verify,
  );
  assert.equal(calls.length, 2);
  for (const [base, candidate, changes] of [
    [record, null, []],
    [record, { ...record, "action-version": "v1.2.2" }, []],
    [
      record,
      { ...record, "action-revision": "2".repeat(40) },
      ["docs/.edgezero-action-release.json"],
    ],
    [null, record, ["code.rs"]],
    [
      null,
      record,
      ["docs/.edgezero-action-release.json", ".github/workflows/a.yml"],
    ],
  ])
    assert.throws(() => checkTransition(base, candidate, changes, verify));
  assert.throws(() =>
    checkTransition(null, record, ["docs/a.md"], () => {
      throw Error("unverified release");
    }),
  );
});

test("event ranges are selected from exact hosted context", () => {
  const base = "2".repeat(40),
    head = "3".repeat(40),
    app = "4".repeat(40);
  const git = {
    parents: () => [base, app],
    ancestor: (a, b) => a === base && b === head,
  };
  const env = {
    GITHUB_EVENT_NAME: "pull_request",
    GITHUB_SHA: head,
    GITHUB_REF: "refs/pull/9/merge",
  };
  const event = {
    number: 9,
    pull_request: {
      base: {
        sha: base,
        ref: "main",
        repo: { full_name: "stackpop/edgezero" },
      },
      head: { sha: app },
    },
  };
  assert.deepEqual(selectRange(env, event, git), { base, candidate: head });
  assert.throws(() =>
    selectRange({ ...env, GITHUB_REF: "refs/heads/main" }, event, git),
  );
  assert.throws(
    () => selectRange(env, event, { ...git, parents: () => [app, base] }),
    /synthetic merge parents differ: expected .*; observed /,
  );
  assert.throws(() =>
    selectRange({ ...env, GITHUB_EVENT_NAME: "workflow_dispatch" }, event, git),
  );
  assert.deepEqual(
    selectRange(
      {
        GITHUB_EVENT_NAME: "push",
        GITHUB_SHA: head,
        GITHUB_WORKFLOW_SHA: head,
        GITHUB_REF: "refs/heads/main",
      },
      { before: base, after: head },
      git,
    ),
    { base, candidate: head },
  );
  const group = {
    action: "checks_requested",
    merge_group: {
      base_sha: base,
      head_sha: head,
      base_ref: "refs/heads/main",
      head_ref: "refs/heads/gh-readonly-queue/main/pr-9",
    },
  };
  assert.deepEqual(
    selectRange(
      {
        GITHUB_EVENT_NAME: "merge_group",
        GITHUB_SHA: head,
        GITHUB_REF: group.merge_group.head_ref,
      },
      group,
      git,
    ),
    { base, candidate: head },
  );
  assert.throws(() =>
    selectRange(
      {
        GITHUB_EVENT_NAME: "merge_group",
        GITHUB_SHA: head,
        GITHUB_REF: group.merge_group.head_ref,
      },
      group,
      { ...git, ancestor: () => false },
    ),
  );
});

test("release proof rejects API/ref substitution and redirects without leaking a token in argv", (t) => {
  const temp = mkdtempSync(resolve(tmpdir(), "edgezero-doc-release-"));
  const original = { ...process.env };
  t.after(() => {
    process.env = original;
    rmSync(temp, { recursive: true, force: true });
  });
  writeFileSync(
    resolve(temp, "curl"),
    '#!/bin/sh\nprintf "%s\\n" "$@" >"$FAKE_ARGS"\ncat >"$FAKE_HEADERS"\nprintf "%s" "$FAKE_REPLY"\n',
    { mode: 0o755 },
  );
  writeFileSync(
    resolve(temp, "git"),
    '#!/bin/sh\nif [ "$GIT_CONFIG_NOSYSTEM" != 1 ] || [ "$GIT_CONFIG_GLOBAL" != /dev/null ] || [ "$GIT_TERMINAL_PROMPT" != 0 ]; then exit 9; fi\n[ "$PWD" = "$HOME" ] || exit 10\n[ -z "${GITHUB_TOKEN+x}" ] || exit 11\ncase "$*" in *http.followRedirects=false*) ;; *) exit 12 ;; esac\nprintf "%s" ' +
      "'" +
      `${revision}\trefs/tags/v1.2.3\n` +
      "'\n",
    { mode: 0o755 },
  );
  process.env.PATH = `${temp}:${original.PATH}`;
  process.env.GITHUB_TOKEN = "fixture-token";
  process.env.FAKE_ARGS = resolve(temp, "args");
  process.env.FAKE_HEADERS = resolve(temp, "headers");
  const release = {
    tag_name: "v1.2.3",
    target_commitish: revision,
    draft: false,
    prerelease: false,
    immutable: true,
  };
  process.env.FAKE_REPLY = `${JSON.stringify(release)}\n200`;
  verifyRelease(record);
  assert.ok(
    !readFileSync(process.env.FAKE_ARGS, "utf8").includes("fixture-token"),
  );
  assert.ok(
    readFileSync(process.env.FAKE_HEADERS, "utf8").includes(
      "Authorization: Bearer fixture-token",
    ),
  );
  assert.ok(
    !readFileSync(process.env.FAKE_ARGS, "utf8")
      .split("\n")
      .some((arg) => arg === "-L" || arg === "--location"),
  );
  for (const change of [
    { draft: true },
    { prerelease: true },
    { immutable: false },
    { target_commitish: "main" },
    { tag_name: "v1.2.4" },
  ]) {
    process.env.FAKE_REPLY = `${JSON.stringify({ ...release, ...change })}\n200`;
    assert.throws(() => verifyRelease(record));
  }
  process.env.FAKE_REPLY = `${JSON.stringify(release)}\n302`;
  assert.throws(() => verifyRelease(record));
  process.env.FAKE_REPLY = `${JSON.stringify(release)}\n200`;
  writeFileSync(
    resolve(temp, "git"),
    `#!/bin/sh\nprintf '%s' '${revision}\trefs/tags/v1.2.3\n${revision}\trefs/heads/v1.2.3\n'\n`,
    { mode: 0o755 },
  );
  assert.throws(() => verifyRelease(record));
  writeFileSync(
    resolve(temp, "git"),
    `#!/bin/sh\nprintf '%s' '${"2".repeat(40)}\trefs/tags/v1.2.3\n'\n`,
    { mode: 0o755 },
  );
  assert.throws(() => verifyRelease(record));
});

test("hosted scanner reads committed snapshots and fails inconsistent events", (t) => {
  const temp = mkdtempSync(resolve(tmpdir(), "edgezero-doc-git-"));
  t.after(() => rmSync(temp, { recursive: true, force: true }));
  const subject = resolve(temp, "subject");
  mkdirSync(resolve(subject, "docs/guide"), { recursive: true });
  const git = (...args) =>
    execFileSync(
      "git",
      [
        "-C",
        subject,
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.test",
        ...args,
      ],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    ).trim();
  git("init", "-q");
  writeFileSync(resolve(subject, path), fence(example()));
  git("add", ".");
  git("commit", "-qm", "base");
  const base = git("rev-parse", "HEAD");
  writeFileSync(resolve(subject, "README.md"), "Unrelated documentation\n");
  git("add", ".");
  git("commit", "-qm", "candidate");
  const head = git("rev-parse", "HEAD");
  // Dirty subject data must not replace the committed candidate being evaluated.
  writeFileSync(resolve(subject, path), fence(example("main")));
  const eventFile = resolve(temp, "event.json");
  writeFileSync(eventFile, JSON.stringify({ before: base, after: head }));
  const env = {
    ...process.env,
    CI: "true",
    GITHUB_ACTIONS: "true",
    GITHUB_EVENT_NAME: "push",
    GITHUB_EVENT_PATH: eventFile,
    GITHUB_SHA: head,
    GITHUB_WORKFLOW_SHA: head,
    GITHUB_REF: "refs/heads/main",
  };
  const checker = fileURLToPath(
    new URL("./check-doc-action-pins.mjs", import.meta.url),
  );
  const run = (changes = {}) =>
    spawnSync(process.execPath, [checker, "--subject-root", subject], {
      env: { ...env, ...changes },
      encoding: "utf8",
    });
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /1 external references; bootstrap/);
  assert.notEqual(run({ GITHUB_WORKFLOW_SHA: base }).status, 0);
  assert.notEqual(run({ GITHUB_EVENT_NAME: "workflow_dispatch" }).status, 0);
  assert.notEqual(run({ CI: "false", GITHUB_ACTIONS: "false" }).status, 0);
});

test("a release transition cannot rename a non-document into documentation", (t) => {
  const subject = mkdtempSync(resolve(tmpdir(), "edgezero-doc-rename-"));
  t.after(() => rmSync(subject, { recursive: true, force: true }));
  mkdirSync(resolve(subject, "docs"));
  const git = (...args) =>
    execFileSync(
      "git",
      [
        "-C",
        subject,
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.test",
        ...args,
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
  git("init", "-q");
  writeFileSync(resolve(subject, "source.rs"), "// A tracked source file\n");
  git("add", ".");
  git("commit", "-qm", "base");
  git("mv", "source.rs", "docs/source.md");
  writeFileSync(resolve(subject, "docs/.edgezero-action-release.json"), bytes);
  const checker = fileURLToPath(
    new URL("./check-doc-action-pins.mjs", import.meta.url),
  );
  const result = spawnSync(
    process.execPath,
    [checker, "--subject-root", subject],
    {
      env: { ...process.env, CI: "false", GITHUB_ACTIONS: "false" },
      encoding: "utf8",
    },
  );
  assert.notEqual(result.status, 0);
  assert.match(
    result.stderr,
    /release transition must change only documentation/,
  );
});
