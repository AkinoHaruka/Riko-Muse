/**
 * Resident DSH Dream dispatcher. The memoryd job ledger remains authoritative;
 * this module only runs one tool-restricted child for a persisted claim and
 * returns its structured proposal for Rust validation.
 */
import { randomUUID } from "node:crypto";
import type { Context } from "@deepseek-ai/cordis";
import type { Agent } from "@deepseek-ai/dsh-agent";
import type { JsonSchemaNode, ObjectJsonSchema } from "@deepseek-ai/dsh-tools";
import type { SubagentRun, SubagentStartRequest } from "@deepseek-ai/dsh-subagent";
import type { MemoryClient } from "./client.js";
import type { Logger } from "./events.js";

type Work = Record<string, unknown> & {
  phase: "extract" | "redecision" | "adjudicate" | "consolidate";
  runner_id: string;
  dream_job_id: string;
  dream_generation: number;
  adjudication_job_id?: string;
  adjudication_generation?: number;
  consolidation_job_id?: string;
  consolidation_generation?: number;
};

type Claimed = { status?: string; work?: Work };

const stringSchema = { type: "string" } as const;
const nullableStringSchema: JsonSchemaNode = {
  oneOf: [{ type: "string" }, { type: "null" }],
};

const schemas: Record<Work["phase"], ObjectJsonSchema> = {
  extract: {
    type: "object",
    properties: {
      candidates: {
        type: "array",
        items: {
          type: "object",
          properties: {
            evidence_id: stringSchema,
            kind: { type: "string", enum: ["fact", "preference", "instruction", "episode"] },
            quote: stringSchema,
            claim: stringSchema,
            occurred_at: nullableStringSchema,
          },
          required: ["evidence_id", "kind", "quote", "claim"],
          additionalProperties: false,
        },
      },
    },
    required: ["candidates"],
    additionalProperties: false,
  },
  redecision: {
    type: "object",
    properties: {
      candidates: {
        type: "array",
        items: {
          type: "object",
          properties: {
            evidence_id: stringSchema,
            kind: { type: "string", enum: ["fact", "preference", "instruction", "episode"] },
            quote: stringSchema,
            claim: stringSchema,
            occurred_at: nullableStringSchema,
          },
          required: ["evidence_id", "kind", "quote", "claim"],
          additionalProperties: false,
        },
      },
    },
    required: ["candidates"],
    additionalProperties: false,
  },
  adjudicate: {
    type: "object",
    properties: {
      results: {
        type: "array",
        items: {
          type: "object",
          properties: {
            candidate_id: stringSchema,
            durability: { type: "string", enum: ["durable", "time_bound", "uncertain", "not_memory"] },
            action: { type: "string", enum: ["create", "attach_evidence", "update", "keep_separate", "conflict", "defer", "not_memory"] },
            reason_code: nullableStringSchema,
            target_memory_id: nullableStringSchema,
            expected_target_version: { oneOf: [{ type: "integer" }, { type: "null" }] },
            model_confidence: { oneOf: [{ type: "number" }, { type: "null" }] },
            valid_until: nullableStringSchema,
          },
          required: ["candidate_id", "durability", "action"],
          additionalProperties: false,
        },
      },
    },
    required: ["results"],
    additionalProperties: false,
  },
  consolidate: {
    type: "object",
    properties: { title: stringSchema, body_md: stringSchema },
    required: ["title", "body_md"],
    additionalProperties: false,
  },
};

function textPrompt(work: Work): string {
  const payload = JSON.stringify(work, null, 2);
  switch (work.phase) {
    case "extract":
    case "redecision":
      return [
        "You are a restricted memory extraction child. Return only the requested structured JSON.",
        "Use only the frozen input events below. Create atomic claims only from user-role evidence.",
        "Copy each quote exactly from one event; evidence_id must identify that same event.",
        "Never invent evidence, alter quotes, write to tools, or claim that a memory was committed.",
        "Output schema: { candidates: [{ evidence_id, kind, quote, claim, occurred_at? }] }.",
        payload,
      ].join("\n\n");
    case "adjudicate":
      return [
        "You are a restricted memory adjudication child. Return only the requested structured JSON.",
        "Judge each frozen candidate against only the supplied recalled targets and evidence.",
        "Choose one allowed action per candidate. Do not invent target IDs or versions.",
        "Rust will independently validate and commit every proposed change.",
        "Output schema: { results: [{ candidate_id, durability, action, reason_code?, target_memory_id?, expected_target_version?, model_confidence?, valid_until? }] }.",
        payload,
      ].join("\n\n");
    case "consolidate":
      return [
        "You are a restricted derived-page writer. Return only title and body_md as structured JSON.",
        "Use only the supplied question/document metadata and frozen source memories.",
        "Do not add facts, source IDs, citations, or links that are not in the input.",
        "Keep the title within 80 characters and body_md within 1200 characters.",
        "Rust rechecks every frozen source before publishing.",
        payload,
      ].join("\n\n");
  }
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const timer = setTimeout(done, ms);
    function done(): void {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    }
    signal.addEventListener("abort", done, { once: true });
  });
}

/** One plugin-process resident loop. It never stores job truth outside memoryd. */
export class DreamRunner {
  private readonly runnerId = `dsh-${randomUUID()}`;
  private controller: AbortController | undefined;
  private task: Promise<void> | undefined;
  private readonly childAgentIds = new Set<string>();
  private disposed = false;
  private lastDiagnosticAt = 0;

  constructor(
    private readonly ctx: Context,
    private readonly client: MemoryClient,
    private readonly logger: Logger,
    private readonly hostId: string,
    private readonly onDispose: () => boolean,
  ) {}

  start(agent: Agent): void {
    if (this.disposed || this.controller || this.childAgentIds.has(String(agent.id))) return;
    const controller = new AbortController();
    this.controller = controller;
    this.task = this.run(agent, controller.signal).finally(() => {
      // Parent agents are session-scoped in DSH. Release the runner slot after
      // one parent disappears so the next legal parent can resume queued work.
      if (this.controller === controller) {
        this.controller = undefined;
        this.task = undefined;
      }
    });
  }

  async dispose(): Promise<void> {
    this.disposed = true;
    this.controller?.abort();
    await this.task?.catch(() => undefined);
  }

  private validParent(agent: Agent): boolean {
    if (this.disposed || this.onDispose()) return false;
    const registry = this.ctx.get("agents") as { get?: (id: string) => Agent | undefined } | undefined;
    return registry?.get?.(String(agent.id)) === agent;
  }

  private async run(agent: Agent, signal: AbortSignal): Promise<void> {
    while (!signal.aborted && this.validParent(agent)) {
      const subagents = this.ctx.subagents;
      if (!subagents || !subagents.list().includes("spawn")) {
        this.diagnostic("missing_runner", "DSH 未注册 spawn 子 Agent provider；Dream 作业保留在 memoryd 队列");
        await sleep(10_000, signal);
        continue;
      }
      const heartbeat = await this.client.dreamRunnerHeartbeat({
        runner_id: this.runnerId,
        host_id: this.hostId,
        agent_id: String(agent.id),
        capabilities: ["chat", "dream_v1", "adjudicate_v1", "consolidate_v1"],
      }, signal);
      if (heartbeat.failure !== "ok" || heartbeat.status < 200 || heartbeat.status >= 300) {
        this.diagnostic("memoryd_unavailable", "Dream runner heartbeat 未确认；作业仍由 memoryd 持久保存");
        await sleep(5_000, signal);
        continue;
      }
      const response = await this.client.dreamRunnerClaim({ runner_id: this.runnerId }, signal);
      if (response.failure !== "ok" || response.status < 200 || response.status >= 300) {
        this.diagnostic("claim_failed", "Dream claim 未确认；runner 将重试");
        await sleep(5_000, signal);
        continue;
      }
      const body = response.body as Claimed;
      if (body.status !== "claimed" || !body.work) {
        await sleep(2_500, signal);
        continue;
      }
      await this.execute(agent, body.work, signal);
    }
  }

  private async execute(agent: Agent, work: Work, parentSignal: AbortSignal): Promise<void> {
    const childAbort = new AbortController();
    const signal = AbortSignal.any([parentSignal, childAbort.signal]);
    let leaseBusy = false;
    let leaseLost = false;
    const renew = async (): Promise<void> => {
      if (leaseBusy || signal.aborted) return;
      leaseBusy = true;
      try {
        if (!this.validParent(agent)) {
          leaseLost = true;
          childAbort.abort();
          return;
        }
        const beat = await this.client.dreamRunnerHeartbeat({
          runner_id: this.runnerId,
          host_id: this.hostId,
          agent_id: String(agent.id),
          capabilities: ["chat", "dream_v1", "adjudicate_v1", "consolidate_v1"],
        }, signal);
        const lease = await this.client.dreamRunnerLease({
          runner_id: this.runnerId,
          dream_job_id: work.dream_job_id,
          dream_generation: work.dream_generation,
          ...(work.adjudication_job_id ? {
            adjudication_job_id: work.adjudication_job_id,
            adjudication_generation: work.adjudication_generation,
          } : {}),
          ...(work.consolidation_job_id ? {
            consolidation_job_id: work.consolidation_job_id,
            consolidation_generation: work.consolidation_generation,
          } : {}),
        }, signal);
        if (beat.failure !== "ok" || beat.status < 200 || beat.status >= 300
          || lease.failure !== "ok" || lease.status < 200 || lease.status >= 300) {
          leaseLost = true;
          childAbort.abort();
        }
      } finally {
        leaseBusy = false;
      }
    };
    const leaseTimer = setInterval(() => { void renew(); }, 15_000);
    let child: SubagentRun | undefined;
    try {
      if (!this.validParent(agent)) return;
      const request: SubagentStartRequest = {
        label: `agent-memory:${work.phase}`,
        prompt: [{ type: "text", text: textPrompt(work) }],
        parent: agent,
        signal,
        maxDepth: 1,
        toolFilter: { allow: [] },
        outputSchema: schemas[work.phase],
      };
      child = await this.ctx.subagents.start("spawn", request);
      this.childAgentIds.add(String(child.id));
      const result = await child.result;
      if (leaseLost || signal.aborted) {
        if (leaseLost && !parentSignal.aborted) {
          await this.reportFailure(work, "RUNNER_DISPATCH_FAILED", parentSignal);
        }
        return;
      }
      if (result.stopReason !== "completed" || result.structured === undefined) {
        await this.reportFailure(work, result.stopReason === "completed" ? "BAD_CHILD_OUTPUT" : "SUBAGENT_FAILED", signal);
        return;
      }
      const output = JSON.stringify(result.structured);
      const submit = await this.submit(work, output, signal);
      if (submit.failure !== "ok" || submit.status < 200 || submit.status >= 300) {
        if (submit.status !== 409) await this.reportFailure(work, "SUBMIT_FAILED", signal);
        this.diagnostic("submit_failed", `Dream ${work.phase} 结果未获 Rust 回执 job=${work.dream_job_id}`);
      }
    } catch {
      if (!parentSignal.aborted && !leaseLost) await this.reportFailure(work, "SUBAGENT_FAILED", signal);
      this.diagnostic("subagent_failed", `受限 Dream 子 Agent 执行失败 phase=${work.phase} job=${work.dream_job_id}`);
    } finally {
      clearInterval(leaseTimer);
      if (child) {
        this.childAgentIds.delete(String(child.id));
        try { await child.dispose(); } catch { /* failed child cleanup is diagnostic-only */ }
      }
    }
  }

  private submit(work: Work, output: string, signal: AbortSignal) {
    const parsed = JSON.parse(output) as Record<string, unknown>;
    if (work.phase === "extract" || work.phase === "redecision") {
      return this.client.dreamSubmitCandidates(work.dream_job_id, {
        runner_id: this.runnerId,
        generation: work.dream_generation,
        output: parsed,
      }, signal);
    }
    if (work.phase === "adjudicate") {
      return this.client.dreamSubmitAdjudication(work.adjudication_job_id ?? "", {
        runner_id: this.runnerId,
        dream_generation: work.dream_generation,
        generation: work.adjudication_generation,
        output: parsed,
      }, signal);
    }
    return this.client.dreamSubmitConsolidation(work.consolidation_job_id ?? "", {
      runner_id: this.runnerId,
      dream_job_id: work.dream_job_id,
      dream_generation: work.dream_generation,
      generation: work.consolidation_generation,
      output: parsed,
    }, signal);
  }

  private async reportFailure(work: Work, code: string, signal: AbortSignal): Promise<void> {
    await this.client.dreamRunnerFailure({
      runner_id: this.runnerId,
      phase: work.phase,
      dream_job_id: work.dream_job_id,
      dream_generation: work.dream_generation,
      ...(work.adjudication_job_id ? {
        adjudication_job_id: work.adjudication_job_id,
        adjudication_generation: work.adjudication_generation,
      } : {}),
      ...(work.consolidation_job_id ? {
        consolidation_job_id: work.consolidation_job_id,
        consolidation_generation: work.consolidation_generation,
      } : {}),
      error_code: code,
    }, signal);
  }

  private diagnostic(key: string, message: string): void {
    const now = Date.now();
    if (now - this.lastDiagnosticAt < 60_000) return;
    this.lastDiagnosticAt = now;
    this.logger.warn(`${key}: ${message}`);
  }
}
