import assert from "node:assert/strict";
import test from "node:test";
import {
  makeBundleHook,
  makePreStepHook,
  makeSoulAssembleHook,
} from "../dist/recall.js";
import { isCapturableSessionHeader, isSubagentSessionHeader } from "../dist/events.js";

const logger = { warn() {}, error() {}, info() {} };
const childAgent = {
  id: "child-session",
  session: { id: "child-session", header: { origin: "subagent", parentSession: "parent-session" } },
};

test("Dream/child sessions receive no Soul, bundle, compose, or L0 capture", async () => {
  let soulCalls = 0;
  let bundleCalls = 0;
  let composeCalls = 0;
  const client = {
    soul: async () => { soulCalls += 1; return { failure: "ok", status: 200, body: { body_md: "soul" } }; },
    contextBundle: async () => { bundleCalls += 1; return { failure: "ok", status: 200, body: { resident: { text: "resident" } } }; },
    compose: async () => { composeCalls += 1; return { failure: "ok", status: 200, body: { text: "memory" } }; },
  };

  const soulHook = makeSoulAssembleHook(client, logger, 100, () => false, "stable-agent");
  const assembly = { sections: [] };
  const assembled = await soulHook(assembly, { agent: childAgent }, async () => assembly);
  assert.deepEqual(assembled.sections, []);

  const decision = { kind: "enter", messages: [] };
  const payload = { agent: childAgent, messages: [], signal: new AbortController().signal };
  const bundle = await makeBundleHook(client, logger, "stable-agent")(payload, async () => decision);
  const compose = await makePreStepHook(client, logger, 100)(payload, async () => decision);
  assert.equal(bundle, decision);
  assert.equal(compose, decision);
  assert.equal(soulCalls, 0);
  assert.equal(bundleCalls, 0);
  assert.equal(composeCalls, 0);
  assert.equal(isSubagentSessionHeader(childAgent.session.header), true);
  assert.equal(isCapturableSessionHeader(childAgent.session.header), false);
});

test("top-level sessions remain eligible for memory context and evidence capture", () => {
  assert.equal(isSubagentSessionHeader({}), false);
  assert.equal(isCapturableSessionHeader({}), true);
  assert.equal(isSubagentSessionHeader({ parentSession: "parent-session" }), true);
  assert.equal(isCapturableSessionHeader(undefined), false);
});
