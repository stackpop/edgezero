import { execFileSync } from "node:child_process";
import {
  existsSync,
  lstatSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
} from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
const require = createRequire(resolve(root, "docs/package.json"));
const MarkdownIt = require("markdown-it");
if (require("markdown-it/package.json").version !== "15.0.1")
  throw Error("markdown-it 15.0.1 is required");
const markdown = new MarkdownIt("commonmark");
const recordPath = "docs/.edgezero-action-release.json";
const placeholder = "<EDGEZERO_ACTION_VERSION>";
const adoption = new Set([
  "docs/specs/edgezero-deploy-github-action.md",
  "docs/specs/edgezero-deploy-action-implementation-plan.md",
  "docs/specs/edgezero-deploy-adoption-guide.md",
  "docs/guide/deploy-github-actions.md",
]);
const versionPattern = /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/;
const isSha = (value) =>
  typeof value === "string" &&
  /^[0-9a-f]{40}$/.test(value) &&
  !/^0+$/.test(value);
const isMarkdown = (path) => /\.(md|markdown)$/i.test(path);
const own = (value, key) =>
  value !== null && typeof value === "object" && Object.hasOwn(value, key);
function requireThat(condition, message) {
  if (!condition) throw Error(message);
}
function command(program, args, options = {}) {
  return execFileSync(program, args, {
    encoding: "utf8",
    maxBuffer: 32 * 1024 * 1024,
    ...options,
  });
}

export function parseRecord(bytes) {
  requireThat(bytes.length <= 16384, "oversized release record");
  const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  const value = JSON.parse(text);
  requireThat(
    value && !Array.isArray(value) && typeof value === "object",
    "release record must be an object",
  );
  requireThat(isSha(value["action-revision"]), "invalid action revision");
  requireThat(
    typeof value["action-version"] === "string" &&
      versionPattern.test(value["action-version"]),
    "invalid action version",
  );
  requireThat(value["schema-version"] === 1, "invalid release schema");
  const canonical = JSON.stringify({
    "action-revision": value["action-revision"],
    "action-version": value["action-version"],
    "schema-version": 1,
  });
  requireThat(
    Buffer.from(canonical).equals(bytes),
    "release record must have exact JCS bytes and fields",
  );
  return value;
}

export function checkTransition(base, candidate, changed, verifyRelease) {
  if (base === null && candidate === null) return;
  requireThat(candidate !== null, "release record cannot be deleted");
  if (base !== null && JSON.stringify(base) === JSON.stringify(candidate))
    return;
  requireThat(
    changed.every((path) => isMarkdown(path) || path === recordPath),
    "release transition must change only documentation",
  );
  if (base !== null) {
    const previous = base["action-version"].slice(1).split(".").map(BigInt);
    const next = candidate["action-version"].slice(1).split(".").map(BigInt);
    const differing = next.findIndex((part, index) => part !== previous[index]);
    requireThat(
      differing !== -1 && next[differing] > previous[differing],
      "release version must strictly increase",
    );
  }
  verifyRelease(candidate);
}

function yamlReferences(document) {
  const result = [];
  const take = (value, job = null, reusable = false) => {
    if (own(value, "uses")) result.push({ ref: value.uses, job, reusable });
  };
  if (Array.isArray(document)) document.forEach((step) => take(step));
  else if (document && typeof document === "object") {
    take(document);
    for (const job of Object.values(document.jobs ?? {})) {
      take(job, job, true);
      for (const step of job?.steps ?? []) take(step, job);
    }
    for (const step of document.steps ?? []) take(step);
    for (const step of document.runs?.steps ?? []) take(step);
  }
  return result;
}

export function scanDocument(path, text, record) {
  let count = 0;
  for (const token of markdown.parse(text, {})) {
    if (
      token.type !== "fence" ||
      !/^(yaml|yml)(?=[\s{:[]|$)/i.test(token.info.trim())
    )
      continue;
    const location = `${path}:${token.map[0] + 1}`;
    if (record !== null)
      requireThat(
        !token.content.includes(placeholder),
        `${location}: released YAML still has an action placeholder`,
      );
    const parsed = command(
      "yq",
      [
        "-o=json",
        "-I=0",
        '{"value": ., "duplicates": [.. | select(kind == "map") | to_entries | group_by(.key) | .[] | select(length > 1)]}',
        "-",
      ],
      { input: token.content },
    );
    let document;
    try {
      document = JSON.parse(parsed);
    } catch {
      throw Error(`${location}: expected one YAML document`);
    }
    requireThat(
      document.duplicates.length === 0,
      `${location}: duplicate YAML keys`,
    );
    if (record !== null)
      requireThat(
        !JSON.stringify(document.value).includes(placeholder),
        `${location}: decoded YAML still has an action placeholder`,
      );
    for (const { ref, job, reusable } of yamlReferences(document.value)) {
      requireThat(
        typeof ref === "string" &&
          ref.length > 0 &&
          !/[\s\u0000-\u001f\u007f]/u.test(ref),
        `${location}: invalid uses value`,
      );
      if (ref.startsWith("./")) continue;
      count += 1;
      if (ref.startsWith("docker://")) {
        requireThat(
          /^docker:\/\/[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$/.test(ref),
          `${location}: Docker action requires a sha256 digest`,
        );
        continue;
      }
      const match =
        /^([A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)*)@([^@]+)$/.exec(
          ref,
        );
      requireThat(match !== null, `${location}: malformed external reference`);
      const [, action, version] = match;
      const edgezero = /^stackpop\/edgezero(?:\/|$)/i.test(action);
      if (edgezero) {
        requireThat(
          reusable
            ? action === "stackpop/edgezero/.github/workflows/build-app-cli.yml"
            : /^stackpop\/edgezero\/\.github\/actions\/[^/]+$/.test(action),
          `${location}: unsupported EdgeZero action or workflow invocation`,
        );
        requireThat(
          job !== null,
          `${location}: EdgeZero examples require a complete job and runner contract`,
        );
        if (reusable)
          requireThat(
            !own(job, "runs-on") && !own(job, "steps"),
            `${location}: reusable caller must omit steps and runs-on`,
          );
        else
          requireThat(
            !own(job, "uses") && job["runs-on"] === "ubuntu-24.04",
            `${location}: public action job requires literal ubuntu-24.04`,
          );
        if (record === null && version === placeholder) {
          requireThat(
            adoption.has(path),
            `${location}: placeholder outside prepublication documents`,
          );
          continue;
        }
        if (record !== null)
          requireThat(
            version === record["action-version"],
            `${location}: action version differs from release record`,
          );
      }
      requireThat(
        versionPattern.test(version),
        `${location}: external reference requires an exact stable patch version`,
      );
    }
  }
  return count;
}

export function selectRange(env, event, git) {
  const candidate = env.GITHUB_SHA;
  requireThat(isSha(candidate), "invalid hosted candidate SHA");
  let base;
  switch (env.GITHUB_EVENT_NAME) {
    case "pull_request": {
      const pr = event.pull_request;
      base = pr?.base?.sha;
      requireThat(
        pr?.base?.repo?.full_name === "stackpop/edgezero" &&
          pr?.base?.ref === "main",
        "wrong pull request base",
      );
      requireThat(
        Number.isSafeInteger(event.number) &&
          event.number > 0 &&
          env.GITHUB_REF === `refs/pull/${event.number}/merge`,
        "wrong pull request merge ref",
      );
      requireThat(
        isSha(base) && isSha(pr?.head?.sha),
        "invalid pull request source SHA",
      );
      requireThat(
        JSON.stringify(git.parents(candidate)) ===
          JSON.stringify([base, pr.head.sha]),
        "synthetic merge parents differ",
      );
      break;
    }
    case "merge_group": {
      const group = event.merge_group;
      base = group?.base_sha;
      requireThat(
        event.action === "checks_requested" &&
          group?.base_ref === "refs/heads/main",
        "wrong merge group event",
      );
      requireThat(
        typeof group?.head_ref === "string" &&
          group.head_ref.startsWith("refs/heads/gh-readonly-queue/main/") &&
          group.head_ref === env.GITHUB_REF &&
          group.head_sha === candidate,
        "wrong merge group head",
      );
      break;
    }
    case "push":
      base = event.before;
      requireThat(
        env.GITHUB_REF === "refs/heads/main" &&
          event.after === candidate &&
          env.GITHUB_WORKFLOW_SHA === candidate,
        "wrong protected-main push",
      );
      break;
    default:
      throw Error("unsupported documentation-gate event");
  }
  requireThat(
    isSha(base) && git.ancestor(base, candidate),
    "invalid or unrelated comparison base",
  );
  return { base, candidate };
}

export function verifyRelease(record) {
  const version = record["action-version"];
  const token = process.env.GITHUB_TOKEN ?? "";
  requireThat(!/[\r\n"\\]/.test(token), "invalid API credential encoding");
  const headers = [
    'header = "Accept: application/vnd.github+json"',
    'header = "X-GitHub-Api-Version: 2022-11-28"',
  ];
  if (token) headers.push(`header = "Authorization: Bearer ${token}"`);
  const reply = command(
    "curl",
    [
      "--disable",
      "--silent",
      "--show-error",
      "--max-time",
      "30",
      "--max-redirs",
      "0",
      "--request",
      "GET",
      "--config",
      "-",
      "--write-out",
      "\n%{http_code}",
      `https://api.github.com/repos/stackpop/edgezero/releases/tags/${version}`,
    ],
    { input: headers.join("\n") + "\n" },
  );
  const split = reply.lastIndexOf("\n");
  requireThat(
    reply.slice(split + 1) === "200",
    "immutable release lookup failed",
  );
  const release = JSON.parse(reply.slice(0, split));
  requireThat(
    release.tag_name === version &&
      release.draft === false &&
      release.prerelease === false &&
      release.immutable === true &&
      release.target_commitish === record["action-revision"],
    "release API identity does not match record",
  );
  const verificationRoot = realpathSync(
    mkdtempSync(resolve(tmpdir(), "edgezero-release-ref-")),
  );
  let refs;
  try {
    refs = command(
      "git",
      [
        "-c",
        "credential.helper=",
        "-c",
        "http.extraHeader=",
        "-c",
        "http.followRedirects=false",
        "ls-remote",
        "https://github.com/stackpop/edgezero.git",
        `refs/tags/${version}`,
        `refs/tags/${version}^{}`,
        `refs/heads/${version}`,
      ],
      {
        cwd: verificationRoot,
        env: {
          PATH: process.env.PATH,
          HOME: verificationRoot,
          GIT_CEILING_DIRECTORIES: dirname(verificationRoot),
          GIT_CONFIG_NOSYSTEM: "1",
          GIT_CONFIG_GLOBAL: "/dev/null",
          GIT_TERMINAL_PROMPT: "0",
        },
      },
    )
      .trim()
      .split("\n");
  } finally {
    rmSync(verificationRoot, { recursive: true, force: true });
  }
  const values = new Map();
  for (const line of refs) {
    const fields = line.split("\t");
    requireThat(
      fields.length === 2 &&
        isSha(fields[0]) &&
        !values.has(fields[1]) &&
        [
          `refs/tags/${version}`,
          `refs/tags/${version}^{}`,
          `refs/heads/${version}`,
        ].includes(fields[1]),
      "invalid remote release refs",
    );
    values.set(fields[1], fields[0]);
  }
  requireThat(
    values.has(`refs/tags/${version}`) &&
      !values.has(`refs/heads/${version}`) &&
      (values.get(`refs/tags/${version}^{}`) ??
        values.get(`refs/tags/${version}`)) === record["action-revision"],
    "anonymous peeled release tag does not match record",
  );
}

function main(args) {
  requireThat(
    args.length === 0 || (args.length === 2 && args[0] === "--subject-root"),
    "usage: check-doc-action-pins.sh [--subject-root PATH]",
  );
  const subject = realpathSync(args[1] ?? root);
  const git = (...args) => command("git", ["-C", subject, ...args]);
  requireThat(
    git("rev-parse", "--show-toplevel").trim() === subject,
    "subject must be a repository root",
  );
  requireThat(
    command("yq", ["--version"]).trim() ===
      "yq (https://github.com/mikefarah/yq/) version v4.53.3",
    "mikefarah yq 4.53.3 is required",
  );
  let range = { base: git("rev-parse", "HEAD").trim(), candidate: null };
  if (process.env.GITHUB_ACTIONS === "true" || process.env.CI === "true") {
    requireThat(
      process.env.GITHUB_ACTIONS === "true" && process.env.GITHUB_EVENT_PATH,
      "hosted GitHub event context is required in CI",
    );
    const event = JSON.parse(
      readFileSync(process.env.GITHUB_EVENT_PATH, "utf8"),
    );
    range = selectRange(process.env, event, {
      parents: (sha) => git("show", "-s", "--format=%P", sha).trim().split(" "),
      ancestor: (base, head) => {
        try {
          git("merge-base", "--is-ancestor", base, head);
          return true;
        } catch {
          return false;
        }
      },
    });
  }
  function pathsAt(sha) {
    return (
      sha === null
        ? git("ls-files", "-z")
        : git("ls-tree", "-rz", "--name-only", sha)
    )
      .split("\0")
      .filter(Boolean);
  }
  function readAt(sha, path) {
    if (sha !== null) {
      const mode = git("ls-tree", sha, "--", path).split(" ")[0];
      requireThat(
        mode === "100644" || mode === "100755",
        `non-regular tracked document: ${path}`,
      );
      return Buffer.from(git("show", `${sha}:${path}`));
    }
    const file = resolve(subject, path);
    const stat = lstatSync(file);
    requireThat(
      stat.isFile() && !stat.isSymbolicLink(),
      `non-regular worktree document: ${path}`,
    );
    return readFileSync(file);
  }
  const basePaths = pathsAt(range.base);
  const candidatePaths = pathsAt(range.candidate);
  const base = basePaths.includes(recordPath)
    ? parseRecord(readAt(range.base, recordPath))
    : null;
  const hasCandidateRecord =
    range.candidate === null
      ? existsSync(resolve(subject, recordPath))
      : candidatePaths.includes(recordPath);
  const candidate = hasCandidateRecord
    ? parseRecord(readAt(range.candidate, recordPath))
    : null;
  const changed = git(
    "diff",
    "--no-renames",
    "--name-only",
    "-z",
    range.base,
    ...(range.candidate === null ? [] : [range.candidate]),
    "--",
  )
    .split("\0")
    .filter(Boolean);
  if (range.candidate === null)
    changed.push(
      ...git("ls-files", "--others", "--exclude-standard", "-z")
        .split("\0")
        .filter(Boolean),
    );
  checkTransition(base, candidate, changed, verifyRelease);
  let references = 0;
  for (const path of candidatePaths.filter(isMarkdown)) {
    if (range.candidate === null && !existsSync(resolve(subject, path)))
      continue;
    references += scanDocument(
      path,
      readAt(range.candidate, path).toString("utf8"),
      candidate,
    );
  }
  requireThat(
    references > 0,
    "documentation gate parsed zero external references",
  );
  console.log(
    `documentation reference policy passed (${references} external references; ${candidate === null ? "bootstrap" : "released"})`,
  );
}

if (
  process.argv[1] &&
  realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`documentation gate: ${error.message}`);
    process.exitCode = 1;
  }
}
