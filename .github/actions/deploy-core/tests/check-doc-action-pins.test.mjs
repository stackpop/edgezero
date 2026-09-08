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

test("bootstrap adoption documents use their existing superpowers locations", () => {
  for (const document of [
    "docs/superpowers/specs/edgezero-deploy-github-action.md",
    "docs/superpowers/plans/edgezero-deploy-action-implementation-plan.md",
    "docs/superpowers/specs/edgezero-deploy-adoption-guide.md",
    path,
  ]) {
    assert.equal(scanDocument(document, fence(example()), null), 1);
  }
  for (const name of [
    "edgezero-deploy-github-action",
    "edgezero-deploy-action-implementation-plan",
    "edgezero-deploy-adoption-guide",
  ]) {
    assert.throws(() =>
      scanDocument(`docs/specs/${name}.md`, fence(example()), null),
    );
  }
  assert.throws(() =>
    scanDocument(
      "docs/superpowers/specs/unreviewed.md",
      fence(example()),
      null,
    ),
  );
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
  for (const text of [
    "The prepublication placeholder was `<EDGEZERO_ACTION_VERSION>`.\n",
    "~~~text\n<EDGEZERO_ACTION_VERSION>\n~~~\n",
  ])
    assert.throws(() =>
      scanDocument(path, text + fence(example("v1.2.3")), record),
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
      "docs/superpowers/**",
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
  const payloadBase = "2".repeat(40),
    prHead = "3".repeat(40),
    candidate = "4".repeat(40),
    firstParent = "5".repeat(40);
  const checkedCommits = [];
  const git = {
    parents: () => [firstParent, prHead],
    ancestor: (base, head) =>
      (base === payloadBase && head === firstParent) ||
      (base === firstParent && head === candidate) ||
      (base === payloadBase && head === candidate),
    availableCommit: (sha) => {
      checkedCommits.push(sha);
      return true;
    },
  };
  const env = {
    GITHUB_EVENT_NAME: "pull_request",
    GITHUB_SHA: candidate,
    GITHUB_REF: "refs/pull/9/merge",
  };
  const event = {
    number: 9,
    pull_request: {
      base: {
        sha: payloadBase,
        ref: "main",
        repo: { full_name: "stackpop/edgezero" },
      },
      head: { sha: prHead },
    },
  };
  assert.deepEqual(selectRange(env, event, git), {
    base: firstParent,
    candidate,
  });
  assert.deepEqual(checkedCommits, [
    payloadBase,
    firstParent,
    prHead,
    candidate,
  ]);
  for (const missing of [payloadBase, firstParent, prHead, candidate])
    assert.throws(
      () =>
        selectRange(env, event, {
          ...git,
          availableCommit: (sha) => sha !== missing,
        }),
      /pull request commit object is unavailable/,
    );
  assert.throws(() =>
    selectRange({ ...env, GITHUB_REF: "refs/heads/main" }, event, git),
  );
  for (const parents of [
    [],
    [firstParent],
    [prHead, firstParent],
    [firstParent, payloadBase],
    [firstParent, prHead, payloadBase],
    ["0".repeat(40), prHead],
  ])
    assert.throws(() =>
      selectRange(env, event, { ...git, parents: () => parents }),
    );
  assert.throws(
    () =>
      selectRange(env, event, {
        ...git,
        ancestor: (base, head) => base === firstParent && head === candidate,
      }),
    /pull request base is not an ancestor of its merge parent/,
  );
  assert.throws(
    () =>
      selectRange(env, event, {
        ...git,
        parents: () => {
          throw Error("missing merge object");
        },
      }),
    /missing merge object/,
  );
  assert.throws(() =>
    selectRange({ ...env, GITHUB_EVENT_NAME: "workflow_dispatch" }, event, git),
  );
  assert.deepEqual(
    selectRange(
      {
        GITHUB_EVENT_NAME: "push",
        GITHUB_SHA: candidate,
        GITHUB_WORKFLOW_SHA: candidate,
        GITHUB_REF: "refs/heads/main",
      },
      { before: payloadBase, after: candidate },
      git,
    ),
    { base: payloadBase, candidate },
  );
  const group = {
    action: "checks_requested",
    merge_group: {
      base_sha: payloadBase,
      head_sha: candidate,
      base_ref: "refs/heads/main",
      head_ref: "refs/heads/gh-readonly-queue/main/pr-9",
    },
  };
  assert.deepEqual(
    selectRange(
      {
        GITHUB_EVENT_NAME: "merge_group",
        GITHUB_SHA: candidate,
        GITHUB_REF: group.merge_group.head_ref,
      },
      group,
      git,
    ),
    { base: payloadBase, candidate },
  );
  assert.throws(() =>
    selectRange(
      {
        GITHUB_EVENT_NAME: "merge_group",
        GITHUB_SHA: candidate,
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
    '#!/bin/sh\nif [ -n "${GITHUB_TOKEN+x}${GH_TOKEN+x}${HTTPS_PROXY+x}${https_proxy+x}${HTTP_PROXY+x}${http_proxy+x}${ALL_PROXY+x}${all_proxy+x}${NO_PROXY+x}${no_proxy+x}${CURL_CA_BUNDLE+x}${SSL_CERT_FILE+x}${HOME+x}${XDG_CONFIG_HOME+x}${CURL_HOME+x}${EDGEZERO_AMBIENT_SENTINEL+x}" ]; then exit 20; fi\n[ "$LC_ALL" = C ] || exit 21\nfixture=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 22\nprintf "%s\\n" "$@" >"$fixture/args"\ncat >"$fixture/headers"\ncat "$fixture/reply"\n',
    { mode: 0o755 },
  );
  writeFileSync(
    resolve(temp, "git"),
    '#!/bin/sh\nif [ "$GIT_CONFIG_NOSYSTEM" != 1 ] || [ "$GIT_CONFIG_GLOBAL" != /dev/null ] || [ "$GIT_TERMINAL_PROMPT" != 0 ]; then exit 9; fi\n[ "$PWD" = "$HOME" ] || exit 10\n[ -z "${GITHUB_TOKEN+x}" ] || exit 11\ncase "$*" in *http.followRedirects=false*) ;; *) exit 12 ;; esac\n[ "$1" = "--no-replace-objects" ] || exit 13\n[ "$GIT_NO_REPLACE_OBJECTS" = 1 ] || exit 14\n[ -z "${GIT_REPLACE_REF_BASE+x}" ] || exit 15\nprintf "%s" ' +
      "'" +
      `${revision}\trefs/tags/v1.2.3\n` +
      "'\n",
    { mode: 0o755 },
  );
  process.env.PATH = `${temp}:${original.PATH}`;
  process.env.GITHUB_TOKEN = "fixture-token";
  process.env.GH_TOKEN = "ambient-gh-token";
  process.env.HTTPS_PROXY = "https://hostile-proxy.invalid";
  process.env.https_proxy = "https://hostile-proxy.invalid";
  process.env.HTTP_PROXY = "http://hostile-proxy.invalid";
  process.env.http_proxy = "http://hostile-proxy.invalid";
  process.env.ALL_PROXY = "socks5://hostile-proxy.invalid";
  process.env.all_proxy = "socks5://hostile-proxy.invalid";
  process.env.NO_PROXY = "api.github.com";
  process.env.no_proxy = "api.github.com";
  process.env.CURL_CA_BUNDLE = resolve(temp, "hostile-ca.pem");
  process.env.SSL_CERT_FILE = resolve(temp, "hostile-cert.pem");
  process.env.HOME = resolve(temp, "hostile-home");
  process.env.XDG_CONFIG_HOME = resolve(temp, "hostile-xdg");
  process.env.CURL_HOME = resolve(temp, "hostile-curl-home");
  process.env.EDGEZERO_AMBIENT_SENTINEL = "must-not-leak";
  process.env.GIT_REPLACE_REF_BASE = "refs/hostile-replacements";
  const release = {
    tag_name: "v1.2.3",
    target_commitish: revision,
    draft: false,
    prerelease: false,
    immutable: true,
  };
  const reply = (
    value = release,
    status = "200",
    selectedVersion = "2026-03-10",
    contentType = "application/json; charset=utf-8",
  ) =>
    `${JSON.stringify(value)}\n${status}\n${selectedVersion}\n${contentType}`;
  const setReply = (value) => writeFileSync(resolve(temp, "reply"), value);
  setReply(reply());
  verifyRelease(record);
  assert.equal(
    readFileSync(resolve(temp, "headers"), "utf8"),
    [
      'header = "Accept: application/vnd.github+json"',
      'header = "X-GitHub-Api-Version: 2026-03-10"',
      'header = "User-Agent: edgezero-build-container-gate/1"',
      'header = "Authorization: Bearer fixture-token"',
      "",
    ].join("\n"),
  );
  assert.ok(
    !readFileSync(resolve(temp, "args"), "utf8").includes("fixture-token"),
  );
  assert.ok(
    readFileSync(resolve(temp, "headers"), "utf8").includes(
      "Authorization: Bearer fixture-token",
    ),
  );
  assert.ok(
    !readFileSync(resolve(temp, "args"), "utf8")
      .split("\n")
      .some((arg) => arg === "-L" || arg === "--location"),
  );
  setReply(
    reply(release, "200", "2026-03-10", "Application/JSON; Charset=UTF-8"),
  );
  verifyRelease(record);
  setReply(reply(release, "200", "2026-03-10", "application/json"));
  verifyRelease(record);
  for (const change of [
    { draft: true },
    { prerelease: true },
    { immutable: false },
    { target_commitish: "main" },
    { tag_name: "v1.2.4" },
  ]) {
    setReply(reply({ ...release, ...change }));
    assert.throws(() => verifyRelease(record));
  }
  for (const response of [
    reply(release, "302"),
    reply(release, "201"),
    reply(release, "200", ""),
    reply(release, "200", "2022-11-28"),
    reply(release, "200", "2026-03-10", "text/json"),
    reply(release, "200", "2026-03-10", "application/json; charset=ascii"),
  ]) {
    setReply(response);
    assert.throws(() => verifyRelease(record));
  }
  setReply(reply());
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

  const proxyBin = resolve(temp, "bin");
  mkdirSync(proxyBin);
  writeFileSync(
    resolve(proxyBin, "git"),
    '#!/bin/sh\n[ "$1" = "--no-replace-objects" ] || exit 91\nif [ -n "${GIT_DIR+x}${GIT_WORK_TREE+x}${GIT_COMMON_DIR+x}${GIT_NAMESPACE+x}${GIT_INDEX_FILE+x}${GIT_OBJECT_DIRECTORY+x}${GIT_ALTERNATE_OBJECT_DIRECTORIES+x}${GIT_SHALLOW_FILE+x}${GIT_CONFIG_COUNT+x}${GIT_CONFIG_KEY_0+x}${GIT_CONFIG_VALUE_0+x}${GIT_REPLACE_REF_BASE+x}${REAL_GIT+x}${XDG_CONFIG_HOME+x}${EDGEZERO_GIT_AMBIENT+x}" ]; then exit 92; fi\nif [ "$LC_ALL" != C ] || [ "$GIT_NO_REPLACE_OBJECTS" != 1 ] || [ "$GIT_CONFIG_NOSYSTEM" != 1 ] || [ "$GIT_CONFIG_GLOBAL" != /dev/null ] || [ "$GIT_TERMINAL_PROMPT" != 0 ] || [ "$HOME" != /dev/null ] || [ "$TMPDIR" != /tmp ] || [ "$GIT_CEILING_DIRECTORIES" = / ]; then exit 93; fi\ncase "$PATH" in *:*) PATH=${PATH#*:} ;; *) exit 94 ;; esac\nexport PATH\nexec git "$@"\n',
    { mode: 0o755 },
  );
  const hardened = run({
    PATH: `${proxyBin}:${process.env.PATH}`,
    HOME: "/hostile-home",
    GIT_CEILING_DIRECTORIES: "/",
    GIT_DIR: "/hostile-git-dir",
    GIT_WORK_TREE: "/hostile-work-tree",
    GIT_COMMON_DIR: "/hostile-common-dir",
    GIT_NAMESPACE: "hostile-namespace",
    GIT_INDEX_FILE: "/hostile-index",
    GIT_OBJECT_DIRECTORY: "/hostile-objects",
    GIT_ALTERNATE_OBJECT_DIRECTORIES: "/hostile-alternates",
    GIT_SHALLOW_FILE: "/hostile-shallow",
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "core.abbrev",
    GIT_CONFIG_VALUE_0: "1",
    GIT_REPLACE_REF_BASE: "refs/hostile-replacements",
    REAL_GIT: "/hostile-git",
    XDG_CONFIG_HOME: "/hostile-xdg",
    EDGEZERO_GIT_AMBIENT: "must-not-leak",
  });
  assert.equal(hardened.status, 0, hardened.stderr);

  const gitDir = resolve(subject, git("rev-parse", "--git-dir"));
  const shallow = resolve(gitDir, "shallow");
  writeFileSync(shallow, `${head}\n`);
  const shallowResult = run();
  assert.notEqual(shallowResult.status, 0);
  assert.match(shallowResult.stderr, /shallow repository/);
  rmSync(shallow);

  git("replace", head, base);
  const replaceResult = run();
  assert.notEqual(replaceResult.status, 0);
  assert.match(replaceResult.stderr, /replace refs/);
  git("replace", "-d", head);

  const grafts = resolve(gitDir, "info/grafts");
  writeFileSync(grafts, `${head} ${base}\n`);
  const graftsResult = run();
  assert.notEqual(graftsResult.status, 0);
  assert.match(graftsResult.stderr, /grafts/);
  rmSync(grafts);

  assert.notEqual(run({ GITHUB_WORKFLOW_SHA: base }).status, 0);
  assert.notEqual(run({ GITHUB_EVENT_NAME: "workflow_dispatch" }).status, 0);
  assert.notEqual(run({ CI: "false", GITHUB_ACTIONS: "false" }).status, 0);
});

test("pull request scanning uses the synthetic merge first parent", (t) => {
  const temp = mkdtempSync(resolve(tmpdir(), "edgezero-doc-pr-"));
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
  git("init", "-q", "-b", "main");
  writeFileSync(resolve(subject, path), fence(example()));
  git("add", ".");
  git("commit", "-qm", "payload base");
  const payloadBase = git("rev-parse", "HEAD");

  git("checkout", "-qb", "feature");
  writeFileSync(resolve(subject, "README.md"), "Pull request documentation\n");
  git("add", ".");
  git("commit", "-qm", "pull request head");
  const prHead = git("rev-parse", "HEAD");

  git("checkout", "-q", "main");
  writeFileSync(resolve(subject, path), fence(example("v1.2.3")));
  writeFileSync(resolve(subject, "docs/.edgezero-action-release.json"), bytes);
  writeFileSync(resolve(subject, "release-helper.sh"), "#!/bin/sh\nexit 0\n", {
    mode: 0o755,
  });
  git("add", ".");
  git("commit", "-qm", "advanced base");
  const firstParent = git("rev-parse", "HEAD");
  git("merge", "--no-ff", "-qm", "synthetic merge", prHead);
  const candidate = git("rev-parse", "HEAD");
  assert.deepEqual(git("show", "-s", "--format=%P", candidate).split(" "), [
    firstParent,
    prHead,
  ]);
  assert.match(
    git("ls-tree", firstParent, "--", "release-helper.sh"),
    /^100755 blob /,
  );
  const staleChanges = git(
    "diff",
    "--no-renames",
    "--name-only",
    payloadBase,
    candidate,
    "--",
  )
    .split("\n")
    .filter(Boolean);
  let releaseVerified = false;
  assert.throws(
    () =>
      checkTransition(null, record, staleChanges, () => {
        releaseVerified = true;
      }),
    /release transition must change only documentation/,
  );
  assert.equal(releaseVerified, false);

  const eventFile = resolve(temp, "event.json");
  writeFileSync(
    eventFile,
    JSON.stringify({
      number: 9,
      pull_request: {
        base: {
          sha: payloadBase,
          ref: "main",
          repo: { full_name: "stackpop/edgezero" },
        },
        head: { sha: prHead },
      },
    }),
  );
  const checker = fileURLToPath(
    new URL("./check-doc-action-pins.mjs", import.meta.url),
  );
  const result = spawnSync(
    process.execPath,
    [checker, "--subject-root", subject],
    {
      env: {
        ...process.env,
        CI: "true",
        GITHUB_ACTIONS: "true",
        GITHUB_EVENT_NAME: "pull_request",
        GITHUB_EVENT_PATH: eventFile,
        GITHUB_SHA: candidate,
        GITHUB_REF: "refs/pull/9/merge",
      },
      encoding: "utf8",
    },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /1 external references; released/);
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
