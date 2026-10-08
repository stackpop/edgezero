import assert from "node:assert/strict";
import test from "node:test";
import { validateEvidence, validateTarget } from "./qualify_response_egress.mjs";

function fixture() {
  const manifest = {
    schema: 1, run: "run-1", revision: "a".repeat(40), artifact: "b".repeat(64),
    adapter: "cloudflare", sdk: "worker 0.8.5", runtime: { unknown: "provider-unpublished" },
    compatibility: { date: "2026-10-07", flags: [] }, deployment: "isolated-test-deployment",
    binary_sha256: "c".repeat(64),
    telemetry_complete: true, expected: [{ probe: "normal-1", case: "normal" }],
  };
  const clients = [{ probe: "normal-1", case: "normal", action: "read", ended: true, bytes: 3072, status: 200 }];
  const common = { schema: 1, run: manifest.run, revision: manifest.revision, artifact: manifest.artifact,
    probe: "normal-1", case: "normal", instance: "instance-1" };
  const events = [
    { ...common, sequence: 0, event: "source-drop" },
    { ...common, sequence: 1, event: "terminal", outcome: "HostHandoff", body_kind: "Application",
      bytes_written: 3072, fallback: null },
    { ...common, sequence: 2, event: "resource-drop" },
    { ...common, sequence: 3, event: "trace-closed", source_drops: 1, resource_drops: 1,
      terminals: 1, polls: 4, polls_after_terminal: 0 },
  ];
  return { manifest, clients, events };
}

test("complete correlated ownership evidence is accepted without claiming client delivery", () => {
  const { manifest, clients, events } = fixture();
  assert.equal(validateEvidence(manifest, clients, events).status, "qualified-ownership");
});

test("missing, duplicated, truncated and mismatched telemetry fails closed", () => {
  for (const mutate of [
    ({ events }) => events.splice(0, 1),
    ({ events }) => events.push({ ...events[1] }),
    ({ events }) => events.pop(),
    ({ events }) => { events[1].revision = "c".repeat(40); },
    ({ events }) => { events[3].polls_after_terminal = 1; },
    ({ events }) => { events[3].source_drops = 2; },
    ({ manifest }) => { manifest.telemetry_complete = false; },
    ({ clients }) => clients.pop(),
    ({ clients }) => { clients[0].action = "nonread"; },
    ({ manifest }) => { manifest.adapter = "invented-target"; },
    ({ manifest }) => { delete manifest.binary_sha256; },
    ({ manifest }) => manifest.expected.push({ probe: "recovery-1", case: "normal" }),
    ({ events }) => { events[1].body_kind = "Fallback"; },
    ({ events }) => { events[1].bytes_written = 0; },
    ({ events }) => { events[3].polls = 0; },
    ({ manifest }) => { manifest.runtime = true; },
    ({ manifest }) => { manifest.compatibility = true; },
    ({ manifest }) => { manifest.binary_sha256 = [manifest.binary_sha256]; },
    ({ manifest, events }) => { delete manifest.run; for (const event of events) delete event.run; },
    ({ manifest, events, clients }) => {
      delete manifest.expected[0].probe; delete clients[0].probe;
      for (const event of events) delete event.probe;
    },
  ]) {
    const value = fixture();
    mutate(value);
    assert.notEqual(validateEvidence(value.manifest, value.clients, value.events).status, "qualified-ownership");
  }
});

test("target requires allowlisting, finite budgets and no unexpected private fields", () => {
  const { schema, expected, telemetry_complete, ...identity } = fixture().manifest;
  const target = { ...identity, origin: "https://isolated.example.com", allowed_origin: "https://isolated.example.com",
    isolated_nonproduction: true, max_probe_ms: 2000 };
  assert.equal(validateTarget(target).origin, target.origin);
  for (const changed of [
    { ...target, origin: "http://isolated.example.com" },
    { ...target, origin: "https://token@isolated.example.com" },
    { ...target, allowed_origin: "https://another.example.com" },
    { ...target, isolated_nonproduction: false },
    { ...target, max_probe_ms: 0 },
    { ...target, private_token: "do-not-persist" },
    { ...target, adapter: "fastly" },
  ]) assert.throws(() => validateTarget(changed));
});

test("bodyless and failure cases enforce source activity and terminal classifications", () => {
  for (const name of ["head", "204", "205", "304", "pending", "expired", "source-error", "conversion-error"]) {
    const value = fixture();
    value.manifest.expected[0].case = name;
    value.clients[0].case = name;
    for (const event of value.events) event.case = name;
    const terminal = value.events[1];
    const closed = value.events[3];
    const client = value.clients[0];
    const bodyless = ["head", "204", "205", "304"].includes(name);
    terminal.bytes_written = name === "source-error" ? 1024 : 0;
    closed.polls = bodyless || ["expired", "conversion-error"].includes(name) ? 0 : 2;
    client.bytes = 0;
    client.status = bodyless ? (name === "head" ? 200 : Number(name)) : null;
    if (!bodyless) {
      terminal.outcome = ({pending: "DeadlineExceeded", expired: "DeadlineExceeded",
        "source-error": "SourceError", "conversion-error": "ConversionError"})[name];
      client.ended = false;
    }
    if (["expired", "conversion-error"].includes(name)) {
      terminal.body_kind = "Fallback";
      terminal.fallback = "Completed";
    }
    assert.equal(validateEvidence(value.manifest, value.clients, value.events).status, "qualified-ownership", name);
    terminal.outcome = "InventedOutcome";
    assert.equal(validateEvidence(value.manifest, value.clients, value.events).status, "unverified", name);
  }
});

test("reordered complete telemetry is accepted; records from another instance are not", () => {
  const value = fixture();
  value.events.reverse();
  assert.equal(validateEvidence(value.manifest, value.clients, value.events).status, "qualified-ownership");
  value.events[0].instance = "another-instance";
  assert.notEqual(validateEvidence(value.manifest, value.clients, value.events).status, "qualified-ownership");
});
