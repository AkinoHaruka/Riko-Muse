import assert from "node:assert/strict";
import test from "node:test";
import { makeBundleHook, segmentText, SEGMENT_FORMS } from "../dist/recall.js";

const logger = { warn() {}, error() {}, info() {} };
const agent = { id: "a1", session: { id: "s1", header: {} } };
const payload = (messages = []) => ({
  agent,
  messages,
  signal: new AbortController().signal,
});
const userMessage = (text) => ({
  source: { kind: "user" },
  content: [{ type: "text", text }],
});
const decision = { kind: "enter", messages: [] };

test("segments are injected in order as separate user-role messages", async () => {
  const client = {
    contextBundle: async () => ({
      failure: "ok",
      status: 200,
      body: {
        resident: { text: "legacy resident" },
        retrieved: { text: "legacy retrieved" },
        segments: [
          { segment: "compact", char_count: 6, items: [{ body: "住在昆明", stable_refs: ["riko://memory/t/u/user_main/m1@1"] }] },
          { segment: "alignment", char_count: 0, version: 0 },
          { segment: "relationships", char_count: 0, items: [] },
          { segment: "retrieved", char_count: 9, text: "retrieved body" },
        ],
      },
    }),
  };
  const out = await makeBundleHook(client, logger, "stable")(payload([userMessage("在吗")]), async () => decision);
  assert.equal(out.kind, "enter");
  // 只有非空段注入：compact + retrieved 两条，alignment/relationships 空段不加占位。
  assert.equal(out.messages.length, 2);
  assert.equal(out.messages[0].source.form, "compact");
  assert.equal(out.messages[1].source.form, "retrieved");
  for (const m of out.messages) assert.equal(m.source.kind, "agent-memory");
  const compactText = out.messages[0].content.map((b) => b.text).join("\n");
  assert.match(compactText, /住在昆明/);
  assert.match(compactText, /riko:\/\/memory\/t\/u\/user_main\/m1@1/);
});

test("missing segments falls back to resident/retrieved (D6 behaviour)", async () => {
  const client = {
    contextBundle: async () => ({
      failure: "ok",
      status: 200,
      body: { resident: { text: "resident body" }, retrieved: { text: "retrieved body" } },
    }),
  };
  const out = await makeBundleHook(client, logger, "stable")(payload([userMessage("在吗")]), async () => decision);
  assert.equal(out.messages.length, 2);
  assert.equal(out.messages[0].source.form, "resident");
  assert.equal(out.messages[1].source.form, "retrieved");
});

test("segmentText preserves verbatim body and refs, never summarises", () => {
  assert.equal(
    segmentText({ segment: "retrieved", text: "  原文  " }),
    "  原文  ",
  );
  const rel = segmentText({
    segment: "relationships",
    items: [{ display_name: "小雨", relation: "妻子", detail_ref: "riko://entity/t/u/user_main/e1" }],
  });
  assert.equal(rel, "- 小雨（妻子） <riko://entity/t/u/user_main/e1>");
  assert.equal(segmentText({ segment: "compact", items: [] }), "");
  assert.deepEqual(SEGMENT_FORMS, ["compact", "alignment", "relationships", "retrieved"]);
});
