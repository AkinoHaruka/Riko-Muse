import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { Context } from "@deepseek-ai/cordis";
import SystemPrompt, { renderPrompt } from "@deepseek-ai/dsh-system-prompt";
import {
  registerTwinSoul,
  TWIN_SOUL_SECTION_NAME,
  TWIN_SOUL_SECTION_ORDER,
} from "../dist/soul.js";

async function withPrompt(soulFile, run) {
  const ctx = new Context();
  try {
    await ctx.plugin(SystemPrompt, {});
    const twinSoul = {
      name: "test-twin-soul",
      inject: ["systemPrompt"],
      apply(pluginCtx) {
        registerTwinSoul(pluginCtx, soulFile);
      },
    };
    await ctx.plugin(twinSoul);
    await run(ctx);
  } finally {
    await ctx.fiber.dispose();
  }
}

async function waitFor(check, timeoutMs = 2000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail("timed out waiting for soul.md hot reload");
}

test("registers twin-soul at order 100 and injects its content alongside API soul", async () => {
  const dir = await mkdtemp(join(tmpdir(), "riko-dsh-soul-"));
  const soulFile = join(dir, "soul.md");
  await writeFile(soulFile, "I am 璃, the user's twin.", "utf8");

  try {
    await withPrompt(soulFile, async (ctx) => {
      ctx.systemPrompt.section({ name: "test:after-twin", order: 101, text: "Ordered anchor." });
      ctx.systemPrompt.section({ name: "riko-dsh:soul", order: 10300, text: "API-backed soul." });
      const assembly = await ctx.systemPrompt.assemble();
      const twin = assembly.sections.find((section) => section.name === TWIN_SOUL_SECTION_NAME);
      const api = assembly.sections.find((section) => section.name === "riko-dsh:soul");
      const twinIndex = assembly.sections.findIndex((section) => section.name === TWIN_SOUL_SECTION_NAME);
      const anchorIndex = assembly.sections.findIndex((section) => section.name === "test:after-twin");
      const apiIndex = assembly.sections.findIndex((section) => section.name === "riko-dsh:soul");

      assert.equal(TWIN_SOUL_SECTION_ORDER, 100);
      assert.equal(twin?.text, "I am 璃, the user's twin.");
      assert.equal(api?.text, "API-backed soul.");
      assert.ok(twinIndex < anchorIndex && anchorIndex < apiIndex, "sections must assemble in their configured order");

      const prompt = renderPrompt(assembly);
      assert.ok(prompt.includes("I am 璃, the user's twin."));
      assert.ok(prompt.includes("API-backed soul."));
    });
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("reloads soul.md changes into the next system prompt assembly", async () => {
  const dir = await mkdtemp(join(tmpdir(), "riko-dsh-soul-watch-"));
  const soulFile = join(dir, "soul.md");
  await writeFile(soulFile, "First personality.", "utf8");

  try {
    await withPrompt(soulFile, async (ctx) => {
      assert.ok(renderPrompt(await ctx.systemPrompt.assemble()).includes("First personality."));
      await writeFile(soulFile, "Updated twin personality.", "utf8");
      await waitFor(async () => renderPrompt(await ctx.systemPrompt.assemble()).includes("Updated twin personality."));
      assert.ok(!renderPrompt(await ctx.systemPrompt.assemble()).includes("First personality."));
    });
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
