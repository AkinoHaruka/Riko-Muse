import { test } from "node:test";
import assert from "node:assert/strict";

const mod = await import("../dist/schedule.js");
const { scheduleCreate, scheduleList, scheduleDelete, scheduleUpdate, scheduleClearAll } = mod;

test("create and list", () => {
  scheduleClearAll();
  const id = scheduleCreate(undefined, {
    kind: "once",
    spec: 60000,
    description: "test task",
    handler: () => {},
  });
  assert.ok(id.startsWith("sched-"));
  const list = scheduleList();
  assert.equal(list.length, 1);
  assert.equal(list[0].description, "test task");
  // handler should not leak into list output
  assert.ok(!("handler" in list[0]));
  scheduleClearAll();
});

test("delete removes task", () => {
  scheduleClearAll();
  const id = scheduleCreate(undefined, {
    kind: "interval",
    spec: 60000,
    description: "to delete",
    handler: () => {},
  });
  assert.equal(scheduleDelete(id), true);
  assert.equal(scheduleList().length, 0);
  assert.equal(scheduleDelete("nonexistent"), false);
  scheduleClearAll();
});

test("update description", () => {
  scheduleClearAll();
  const id = scheduleCreate(undefined, {
    kind: "once",
    spec: 60000,
    description: "old",
    handler: () => {},
  });
  assert.equal(scheduleUpdate(id, { description: "new" }), true);
  assert.equal(scheduleList()[0].description, "new");
  scheduleClearAll();
});

test("once task fires and auto-removes", async () => {
  scheduleClearAll();
  let fired = false;
  const id = scheduleCreate(undefined, {
    kind: "once",
    spec: 50,
    description: "fire test",
    handler: () => { fired = true; },
  });
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(fired, true);
  assert.equal(scheduleList().length, 0);
  scheduleClearAll();
});

test("invalid spec throws", () => {
  scheduleClearAll();
  assert.throws(() => scheduleCreate(undefined, {
    kind: "once",
    spec: "not-a-time",
    description: "bad",
    handler: () => {},
  }));
  scheduleClearAll();
});
