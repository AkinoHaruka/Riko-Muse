import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { Context } from "@deepseek-ai/cordis";
import SystemPrompt, { renderPrompt } from "@deepseek-ai/dsh-system-prompt";
import {
  createFileSoulWatcher,
  registerTwinSoul,
  TWIN_SOUL_SECTION_NAME,
  TWIN_SOUL_SECTION_ORDER,
} from "../dist/soul.js";
import { loadConfig } from "../dist/config.js";
import {
  makeSoulAssembleHook,
  SOUL_SECTION_NAME,
  SOUL_SECTION_ORDER,
} from "../dist/recall.js";

const logger = { warn() {}, error() {}, info() {} };

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

test("B1: fileSoulEnabled defaults to false in config", () => {
  const base = {
    userTokenFile: "/fake/token",
    spoolDir: "/fake/spool",
    hostId: "host-1",
  };
  const cfg = loadConfig(base);
  assert.equal(cfg.fileSoulEnabled, false);
  assert.equal(cfg.soulFile, "");

  const optIn = loadConfig({ ...base, fileSoulEnabled: true, soulFile: "/custom/soul.md" });
  assert.equal(optIn.fileSoulEnabled, true);
  assert.equal(optIn.soulFile, "/custom/soul.md");
});

test("B1: default preset injects exactly one Soul (kernel soul at order 10300) without twin-soul", async () => {
  const client = {
    soul: async (agentId) => ({
      failure: "ok",
      status: 200,
      body: { version: 42, body_md: `Kernel Soul for ${agentId}` },
    }),
  };
  const hook = makeSoulAssembleHook(client, logger, 300, () => false, "riko");
  const assembly = { sections: [] };
  const context = { agent: { id: "session-abc" } };
  const result = await hook(assembly, context, async () => assembly);

  // Exactly one Soul section
  const soulSections = result.sections.filter((s) => s.name === SOUL_SECTION_NAME || s.name === TWIN_SOUL_SECTION_NAME);
  assert.equal(soulSections.length, 1);
  assert.equal(soulSections[0].name, SOUL_SECTION_NAME);
  assert.equal(soulSections[0].order, SOUL_SECTION_ORDER);
  assert.equal(SOUL_SECTION_ORDER, 10300);
  assert.ok(soulSections[0].text.includes("Kernel Soul for riko"));
  // twin-soul (order 100) must NOT exist
  assert.equal(result.sections.some((s) => s.name === TWIN_SOUL_SECTION_NAME), false);
});

test("B1: multi-agent Soul is isolated per agent (agents cannot see each other's Soul)", async () => {
  const client = {
    soul: async (agentId) => {
      if (agentId === "agent-alpha") {
        return { failure: "ok", status: 200, body: { version: 1, body_md: "Alpha personality: methodical & precise." } };
      }
      if (agentId === "agent-beta") {
        return { failure: "ok", status: 200, body: { version: 2, body_md: "Beta personality: creative & adventurous." } };
      }
      return { failure: "not_found", status: 404 };
    },
  };

  const hook = makeSoulAssembleHook(client, logger, 300, () => false, "");

  // Agent Alpha assemble
  const assemblyAlpha = { sections: [] };
  const resultAlpha = await hook(assemblyAlpha, { agent: { id: "agent-alpha" } }, async () => assemblyAlpha);
  const alphaSoul = resultAlpha.sections.find((s) => s.name === SOUL_SECTION_NAME);
  assert.ok(alphaSoul?.text.includes("Alpha personality"));
  assert.ok(!alphaSoul?.text.includes("Beta personality"));

  // Agent Beta assemble
  const assemblyBeta = { sections: [] };
  const resultBeta = await hook(assemblyBeta, { agent: { id: "agent-beta" } }, async () => assemblyBeta);
  const betaSoul = resultBeta.sections.find((s) => s.name === SOUL_SECTION_NAME);
  assert.ok(betaSoul?.text.includes("Beta personality"));
  assert.ok(!betaSoul?.text.includes("Alpha personality"));

  // Subagent/Dream child session receives NO Soul
  const assemblyChild = { sections: [] };
  const resultChild = await hook(
    assemblyChild,
    { agent: { id: "agent-alpha", session: { header: { origin: "subagent", parentSession: "p" } } } },
    async () => assemblyChild,
  );
  assert.equal(resultChild.sections.length, 0);
});

test("B1: opt-in file soul walks assemble path at order 10300 and preserves agent isolation", async () => {
  const dir = await mkdtemp(join(tmpdir(), "riko-dsh-file-soul-"));
  const soulFile = join(dir, "soul.md");
  await writeFile(soulFile, "Overridden file soul for primary agent.", "utf8");

  try {
    let watcher;
    const client = {
      soul: async (agentId) => ({
        failure: "ok",
        status: 200,
        body: { version: 10, body_md: `Kernel fallback for ${agentId}` },
      }),
    };

    watcher = createFileSoulWatcher(soulFile);
    const hook = makeSoulAssembleHook(
      client,
      logger,
      300,
      () => false,
      "primary-agent",
      {
        enabled: true,
        targetAgent: "primary-agent",
        getSoulText: () => watcher.getText(),
      },
    );

    // Primary agent gets file soul via assemble path at order 10300
    const assemblyPrimary = { sections: [] };
    const resPrimary = await hook(assemblyPrimary, { agent: { id: "primary-agent" } }, async () => assemblyPrimary);
    const primarySoul = resPrimary.sections.filter((s) => s.name === SOUL_SECTION_NAME || s.name === TWIN_SOUL_SECTION_NAME);
    assert.equal(primarySoul.length, 1);
    assert.equal(primarySoul[0].name, SOUL_SECTION_NAME);
    assert.equal(primarySoul[0].order, 10300);
    assert.ok(primarySoul[0].text.includes("Overridden file soul for primary agent."));
    // No twin-soul at order 100
    assert.equal(resPrimary.sections.some((s) => s.name === TWIN_SOUL_SECTION_NAME), false);

    // Secondary agent is isolated from primary agent's file soul
    const assemblySecondary = { sections: [] };
    const resSecondary = await hook(assemblySecondary, { agent: { id: "secondary-agent" } }, async () => assemblySecondary);
    const secSoul = resSecondary.sections.find((s) => s.name === SOUL_SECTION_NAME);
    assert.ok(secSoul?.text.includes("Kernel fallback for secondary-agent"));
    assert.ok(!secSoul?.text.includes("Overridden file soul"));

    // Subagent child is isolated even when opt-in is enabled
    const assemblyChild = { sections: [] };
    const resChild = await hook(
      assemblyChild,
      { agent: { id: "primary-agent", session: { header: { origin: "subagent", parentSession: "p" } } } },
      async () => assemblyChild,
    );
    assert.equal(resChild.sections.length, 0);

    watcher.close();
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
