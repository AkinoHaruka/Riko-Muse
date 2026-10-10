/** File-based relationship guidance injected into each top-level DSH pre-step. */
import { readFile, readFileSync, watch, type FSWatcher } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createUserMessage } from "@deepseek-ai/dsh-llm";
import type { UserMessage } from "@deepseek-ai/dsh-llm";
import type { Context } from "@deepseek-ai/cordis";
import type { PreStepDecision } from "@deepseek-ai/dsh-agent";
import type { PreStepPayload } from "./recall.js";
import { PLUGIN_KIND } from "./recall.js";
import { isSubagentSessionHeader, type Logger } from "./events.js";

export const ALIGNMENT_MESSAGE_KIND = "riko-dsh-alignment";

declare module "@deepseek-ai/dsh-llm" {
  interface MessageSourceMap {
    /** Local relationship guidance; never treated as direct user evidence. */
    "riko-dsh-alignment": { kind: "riko-dsh-alignment" };
  }
}

/** Default template lives beside the package's dist/ directory and is user-editable. */
export function defaultAlignmentFilePath(): string {
  const configured = process.env.RIKO_DSH_ALIGNMENT_FILE;
  return configured
    ? resolve(configured)
    : fileURLToPath(new URL("../alignment.md", import.meta.url));
}

function escapeXmlData(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function alignmentMessage(text: string): UserMessage {
  return createUserMessage({
    content: [{ type: "text", text: `<twin_alignment>\n${escapeXmlData(text)}\n</twin_alignment>` }],
    source: { kind: ALIGNMENT_MESSAGE_KIND },
  });
}

/**
 * Place alignment after any riko-memory message and before the original user
 * message when memory was prepended. The legacy compose hook appends its memory
 * message, so this keeps alignment after it as well.
 */
function insertAfterMemory(messages: readonly UserMessage[], addition: UserMessage): UserMessage[] {
  let lastMemory = -1;
  for (let i = 0; i < messages.length; i++) {
    if (messages[i]?.source?.kind === PLUGIN_KIND) lastMemory = i;
  }
  if (lastMemory >= 0) {
    return [...messages.slice(0, lastMemory + 1), addition, ...messages.slice(lastMemory + 1)];
  }

  const firstUser = messages.findIndex((message) => message.source?.kind === "user");
  const index = firstUser >= 0 ? firstUser : messages.length;
  return [...messages.slice(0, index), addition, ...messages.slice(index)];
}

/** Build the pre-step middleware; reading text through a getter enables hot reload. */
export function makeAlignmentPreStepHook(getText: () => string) {
  return async function alignmentPreStep(
    payload: PreStepPayload,
    next: () => Promise<PreStepDecision>,
  ): Promise<PreStepDecision> {
    const decision = await next();
    if (
      decision.kind !== "enter"
      || payload.signal.aborted
      || isSubagentSessionHeader(payload.agent.session.header)
      || decision.messages.some((message) => message.source?.kind === ALIGNMENT_MESSAGE_KIND)
    ) return decision;

    const text = getText().trim();
    if (!text) return decision;
    return { ...decision, messages: insertAfterMemory(decision.messages, alignmentMessage(text)) };
  };
}

function warn(ctx: Context, message: string): void {
  const logger = (ctx as unknown as { logger?: Logger }).logger;
  logger?.warn?.(`riko-dsh: ${message}`);
}

/** Register the live alignment file, pre-step hook, and watcher on the plugin fiber. */
export function registerAlignment(ctx: Context, alignmentFile = defaultAlignmentFilePath()): void {
  const path = resolve(alignmentFile);

  ctx.effect(() => {
    let alignmentText = "";
    try {
      alignmentText = readFileSync(path, "utf8");
    } catch (error) {
      warn(ctx, `alignment 文件无法读取: ${path} (${error instanceof Error ? error.message : String(error)})`);
    }

    const hook = makeAlignmentPreStepHook(() => alignmentText);
    const disposeHook = (ctx as unknown as {
      on: (name: string, callback: unknown) => () => void;
    }).on("agent/pre-step", (async (
      payload: PreStepPayload,
      next: () => Promise<PreStepDecision>,
    ) => hook(payload, next)) as never);

    let watcher: FSWatcher | undefined;
    let reloadTimer: ReturnType<typeof setTimeout> | undefined;

    const reload = () => {
      readFile(path, "utf8", (error, nextText) => {
        if (error) {
          if (error.code === "ENOENT") {
            nextText = "";
          } else {
            warn(ctx, `alignment 文件重载失败: ${path} (${error.message})`);
            return;
          }
        }
        if (nextText === alignmentText) return;
        alignmentText = nextText;
      });
    };

    try {
      watcher = watch(dirname(path), (_eventType, filename) => {
        if (filename !== null && filename.toString() !== basename(path)) return;
        if (reloadTimer !== undefined) clearTimeout(reloadTimer);
        reloadTimer = setTimeout(() => {
          reloadTimer = undefined;
          reload();
        }, 40);
      });
      watcher.on("error", (error) => warn(ctx, `alignment 文件监听失败: ${error.message}`));
    } catch (error) {
      warn(ctx, `alignment 文件监听无法启动: ${path} (${error instanceof Error ? error.message : String(error)})`);
    }

    return () => {
      disposeHook();
      if (reloadTimer !== undefined) clearTimeout(reloadTimer);
      watcher?.close();
    };
  }, "riko-dsh.alignment");
}
