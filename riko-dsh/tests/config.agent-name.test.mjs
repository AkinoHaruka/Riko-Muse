import assert from "node:assert/strict";
import test from "node:test";
import { loadConfig } from "../dist/config.js";

const base = {
  memoryUrl: "http://127.0.0.1:8791",
  userTokenFile: "C:\\test\\memory.token",
  spoolDir: "C:\\test\\spool",
  hostId: "dsh-test",
};

test("v6 context bundle requires stable agentName", () => {
  assert.throws(
    () => loadConfig({ ...base, contextBundleEnabled: true }),
    /必须配置稳定 agentName/,
  );
  assert.equal(
    loadConfig({ ...base, contextBundleEnabled: true, agentName: "riko-main" }).agentName,
    "riko-main",
  );
});

test("legacy injection may omit agentName", () => {
  assert.equal(loadConfig(base).agentName, "");
});

test("context bundle trims stable agentName and rejects whitespace-only values", () => {
  assert.equal(
    loadConfig({ ...base, contextBundleEnabled: true, agentName: "  riko-main  " }).agentName,
    "riko-main",
  );
  assert.throws(
    () => loadConfig({ ...base, contextBundleEnabled: true, agentName: "   " }),
    /必须配置稳定 agentName/,
  );
});
