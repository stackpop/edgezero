import { writeFile } from "node:fs/promises";
import { createReadStream } from "node:fs";
import { request } from "node:https";
import { pathToFileURL } from "node:url";

export const CASES = ["normal", "head", "204", "205", "304", "slow", "nonread",
  "disconnect", "cancel", "pending", "expired", "source-error", "conversion-error"];
const TOKEN = /^[a-z0-9-]{1,64}$/;
const MAX_INPUT = 4 * 1024 * 1024;
const MAX_TRANSFER = 64 * 1024 * 1024;

function token(value) {
  return typeof value === "string" && TOKEN.test(value);
}

function object(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function hex(value, length) {
  return typeof value === "string" && value.length === length && /^[a-f0-9]+$/.test(value);
}

function clientAction(name) {
  return ["nonread", "disconnect", "cancel", "slow"].includes(name) ? name : "read";
}

function validIdentity(spec) {
  return token(spec.run) && hex(spec.revision, 40) &&
    hex(spec.artifact, 64) && hex(spec.binary_sha256, 64) &&
    ["axum", "cloudflare", "fastly", "spin"].includes(spec.adapter) &&
    typeof spec.sdk === "string" && spec.sdk.length > 0 && spec.sdk.length < 80 &&
    object(spec.runtime) &&
    ((typeof spec.runtime.revision === "string" && spec.runtime.revision.length > 0 && spec.runtime.revision.length <= 100) ||
      ["provider-unpublished", "operator-unavailable"].includes(spec.runtime.unknown)) &&
    object(spec.compatibility) && Array.isArray(spec.compatibility.flags) && spec.compatibility.flags.length <= 32 &&
    spec.compatibility.flags.every(flag => typeof flag === "string" && /^[a-zA-Z0-9_-]{1,80}$/.test(flag)) &&
    (spec.adapter !== "cloudflare" || (typeof spec.compatibility.date === "string" && /^\d{4}-\d{2}-\d{2}$/.test(spec.compatibility.date))) &&
    typeof spec.deployment === "string" &&
    spec.deployment.length > 0 && spec.deployment.length <= 200;
}

function requireFact(condition, reason) {
  if (!condition) throw new Error(reason);
}

export function validateTarget(spec) {
  const fields = new Set(["origin", "allowed_origin", "isolated_nonproduction", "run", "adapter", "revision",
    "artifact", "binary_sha256", "sdk", "runtime", "compatibility", "deployment", "max_probe_ms", "execution_ceiling_ms"]);
  requireFact(Object.keys(spec).every(key => fields.has(key)), "Unexpected target field");
  const url = new URL(spec.origin);
  requireFact(url.protocol === "https:" && !url.username && !url.password &&
    url.pathname === "/" && !url.search && !url.hash && url.origin === spec.allowed_origin,
  "An explicitly allowlisted HTTPS origin without credentials or a path is required");
  requireFact(validIdentity(spec),
  "SDK, runtime, compatibility and independently recorded deployment identity are required");
  requireFact(Number.isInteger(spec.max_probe_ms) && spec.max_probe_ms >= 1000 &&
    spec.max_probe_ms <= 10_000, "Probe time budget must be 1000..10000 ms");
  requireFact(spec.isolated_nonproduction === true, "A designated isolated nonproduction target is required");
  requireFact(spec.adapter !== "fastly" || (Number.isInteger(spec.execution_ceiling_ms) &&
    spec.execution_ceiling_ms > 0 && spec.execution_ceiling_ms <= 60_000),
  "Fastly pending-source probes require an independently enforced finite execution ceiling");
  return url;
}

export function validateEvidence(manifest, clients, events) {
  try {
    requireFact(manifest.schema === 1 && validIdentity(manifest),
    "Revision-pinned deployment metadata is incomplete");
    requireFact(manifest.telemetry_complete === true, "Complete unsampled telemetry export is required");
    requireFact(Array.isArray(manifest.expected) && manifest.expected.length > 0 &&
      manifest.expected.length <= 2 * CASES.length, "Invalid expected probe set");
    const expected = new Map();
    for (const probe of manifest.expected) {
      requireFact(token(probe.probe) && CASES.includes(probe.case) && !expected.has(probe.probe),
        "Invalid or duplicate scheduled probe");
      expected.set(probe.probe, probe.case);
    }
    requireFact(clients.length === expected.size, "Missing or extra client observations");
    const observations = new Map();
    for (const client of clients) {
      requireFact(expected.get(client.probe) === client.case && !observations.has(client.probe) &&
        client.action === clientAction(client.case) &&
        Number.isSafeInteger(client.bytes) && client.bytes >= 0 && client.bytes <= MAX_TRANSFER,
      "Invalid client observation");
      observations.set(client.probe, client);
    }
    requireFact(events.length <= 4 * expected.size, "Unexpected or duplicate telemetry");
    for (const event of events) {
      requireFact(event.schema === 1 && event.run === manifest.run && event.revision === manifest.revision &&
        event.artifact === manifest.artifact && expected.get(event.probe) === event.case &&
        token(event.instance), "Telemetry identity mismatch");
    }
    for (const [probe, name] of expected) {
      const trace = events.filter(event => event.probe === probe).sort((a, b) => a.sequence - b.sequence);
      requireFact(trace.length === 4 && trace.every((event, index) =>
        event.sequence === index && event.instance === trace[0].instance), "Incomplete or conflicting trace sequence");
      const one = kind => {
        const selected = trace.filter(event => event.event === kind);
        requireFact(selected.length === 1, "Missing or duplicate lifecycle event");
        return selected[0];
      };
      one("source-drop");
      const terminal = one("terminal");
      const resource = one("resource-drop");
      const closed = one("trace-closed");
      requireFact(closed.sequence === 3 && closed.terminals === 1 && closed.source_drops === 1 &&
        closed.resource_drops === 1 && closed.polls_after_terminal === 0 &&
        Number.isSafeInteger(closed.polls) && closed.polls >= 0 && terminal.sequence < resource.sequence,
      "Ownership counters or release ordering disagree");
      requireFact(Number.isSafeInteger(terminal.bytes_written) && terminal.bytes_written >= 0 &&
        terminal.bytes_written <= MAX_TRANSFER && ["Application", "Fallback"].includes(terminal.body_kind),
      "Invalid terminal byte accounting");
      requireFact(["Completed", "HostHandoff", "ClientDisconnected", "ConversionError", "DeadlineExceeded",
        "RequestCancelled", "SourceError", "TransportError"].includes(terminal.outcome), "Unknown terminal outcome");
      const bodyless = ["head", "204", "205", "304"].includes(name);
      const client = observations.get(probe);
      if (["normal", "head", "204", "205", "304"].includes(name)) {
        requireFact(["Completed", "HostHandoff"].includes(terminal.outcome) &&
          terminal.body_kind === "Application" && terminal.fallback === null && client.ended === true &&
          client.bytes === (bodyless ? 0 : 3072) && client.status === (bodyless && name !== "head" ? Number(name) : 200),
        "Normal/bodyless client observation or terminal outcome disagrees");
      }
      if (bodyless) requireFact(closed.polls === 0 && terminal.bytes_written === 0, "Suppressed body was polled or written");
      if (name === "normal") requireFact(closed.polls >= 4 && terminal.bytes_written === 3072,
        "Normal response lacks source/accepted-byte evidence");
      if (name === "pending") requireFact(closed.polls >= 1, "Pending producer was never polled");
      if (name === "expired") requireFact(closed.polls === 0 && terminal.body_kind === "Fallback" &&
        (client.status === null || client.status === 504), "Expired precommit response was not suppressed");
      if (["nonread", "disconnect", "cancel"].includes(name)) requireFact(client.ended === false &&
        client.result === (name === "disconnect" ? "client-close" : "client-deadline"),
      "Client did not exercise the scheduled interruption");
      if (name === "slow") requireFact(Number.isSafeInteger(client.pauses) && client.pauses > 0,
        "Slow-client backpressure was not exercised");
      if (["expired", "pending"].includes(name)) requireFact(terminal.outcome === "DeadlineExceeded", "Deadline was not observed");
      if (name === "source-error") requireFact(terminal.outcome === "SourceError", "Source failure was misclassified");
      if (name === "conversion-error") requireFact(terminal.outcome === "ConversionError", "Conversion failure was misclassified");
      if (terminal.body_kind === "Fallback") requireFact(["Completed", "Aborted"].includes(terminal.fallback) &&
        ["DeadlineExceeded", "ConversionError", "SourceError"].includes(terminal.outcome) && terminal.bytes_written <= 1024,
      "Fallback cause or bounded disposition is invalid");
      else requireFact(terminal.fallback === null, "Application report cannot have fallback disposition");
    }
    return { status: "qualified-ownership", scope: "scheduled probes only; not confirmed client delivery or provider allocation",
      probes: expected.size, capabilities_changed: false };
  } catch (error) {
    return { status: "unverified", reason: error.message };
  }
}

async function readJson(path) {
  const chunks = [];
  let length = 0;
  for await (const chunk of createReadStream(path, { highWaterMark: 16 * 1024 })) {
    length += chunk.length;
    requireFact(length <= MAX_INPUT, "Evidence input exceeds 4 MiB");
    chunks.push(chunk);
  }
  const bytes = Buffer.concat(chunks, length);
  return JSON.parse(bytes.toString("utf8"));
}

function runClient(origin, spec, probe, remaining) {
  return new Promise(resolve => {
    let bytes = 0;
    let status = null;
    let settled = false;
    let pauseTimer;
    let pauses = 0;
    const action = clientAction(probe.case);
    const started = performance.now();
    const finish = (ended, result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(pauseTimer);
      req.destroy();
      resolve({ ...probe, action, ended, result, status, bytes, pauses, elapsed_ms: Math.ceil(performance.now() - started) });
    };
    const req = request(new URL(`/qualification/${probe.case}`, origin), {
      method: probe.case === "head" ? "HEAD" : "GET", agent: false,
      headers: { "x-qualification-run": spec.run, "x-qualification-probe": probe.probe, "accept-encoding": "identity" },
    }, response => {
      status = response.statusCode;
      response.on("error", () => finish(false, "response-error"));
      response.on("end", () => finish(true, "end"));
      if (action === "nonread") { response.pause(); return; }
      if (action === "disconnect") { finish(false, "client-close"); return; }
      response.on("data", chunk => {
        bytes += chunk.length;
        if (bytes > remaining) { finish(false, "transfer-cap"); return; }
        if (action === "slow") {
          pauses++;
          response.pause();
          pauseTimer = setTimeout(() => response.resume(), 50);
        }
      });
    });
    const timer = setTimeout(() => finish(false, "client-deadline"), action === "cancel" ? 100 : spec.max_probe_ms);
    req.on("error", () => finish(false, "request-error"));
    req.end();
  });
}

async function main(args) {
  if (args[0] === "--execute-hosted" && args.length === 3) {
    const spec = await readJson(args[1]);
    const origin = validateTarget(spec);
    const expected = CASES.flatMap((name, index) => [
      { probe: `case-${index}`, case: name }, { probe: `recovery-${index}`, case: "normal" },
    ]);
    const clients = [];
    let remaining = MAX_TRANSFER;
    for (const probe of expected) {
      requireFact(remaining > 0, "Run transfer budget exhausted");
      const observed = await runClient(origin, spec, probe, remaining);
      clients.push(observed);
      remaining -= observed.bytes;
      if (spec.adapter === "fastly" && ["pending", "cancel", "nonread", "disconnect"].includes(probe.case)) {
        const teardown = Math.max(0, spec.execution_ceiling_ms - observed.elapsed_ms) + 100;
        await new Promise(resolve => setTimeout(resolve, teardown));
      }
    }
    await writeFile(args[2], JSON.stringify({ manifest: { ...spec, schema: 1, expected, telemetry_complete: false }, clients }, null, 2), { flag: "wx", mode: 0o600 });
    process.stdout.write("Client observations recorded; hosted qualification requires a complete correlated telemetry export.\n");
    return 2;
  }
  requireFact(args[0] === "--validate" && args.length === 3,
    "Use --execute-hosted TARGET.json CLIENTS.json or --validate CLIENTS.json EVENTS.json");
  const { manifest, clients } = await readJson(args[1]);
  const events = await readJson(args[2]);
  const result = validateEvidence(manifest, clients, events);
  process.stdout.write(`${JSON.stringify(result)}\n`);
  return result.status === "qualified-ownership" ? 0 : 2;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).then(code => { process.exitCode = code; }).catch(() => {
    process.stderr.write("Hosted lifecycle qualification failed; check the bounded target/evidence inputs.\n");
    process.exitCode = 1;
  });
}
