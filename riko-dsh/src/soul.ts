/** File-based twin personality injected into the DSH system prompt. */
import { readFile, readFileSync, watch, type FSWatcher } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { Context } from "@deepseek-ai/cordis";
import type { PromptSection } from "@deepseek-ai/dsh-system-prompt";

export const TWIN_SOUL_SECTION_NAME = "twin-soul";
export const TWIN_SOUL_SECTION_ORDER = 100;

/** Default template lives beside the package's dist/ directory and is user-editable. */
export function defaultSoulFilePath(): string {
  const configured = process.env.RIKO_DSH_SOUL_FILE;
  return configured
    ? resolve(configured)
    : fileURLToPath(new URL("../soul.md", import.meta.url));
}

function warn(ctx: Context, message: string): void {
  const logger = (ctx as unknown as { logger?: { warn?: (text: string) => void } }).logger;
  logger?.warn?.(`riko-dsh: ${message}`);
}

/** Register the live soul section and release its file watcher with the plugin. */
export function registerTwinSoul(ctx: Context, soulFile = defaultSoulFilePath()): void {
  const path = resolve(soulFile);

  ctx.effect(() => {
    let soulText = "";
    try {
      soulText = readFileSync(path, "utf8");
    } catch (error) {
      warn(ctx, `soul 文件无法读取: ${path} (${error instanceof Error ? error.message : String(error)})`);
    }

    const removeSection = ctx.systemPrompt.section({
      name: TWIN_SOUL_SECTION_NAME,
      order: TWIN_SOUL_SECTION_ORDER,
      text: () => soulText,
      interpolate: false,
    } satisfies PromptSection);

    let watcher: FSWatcher | undefined;
    let reloadTimer: ReturnType<typeof setTimeout> | undefined;

    const reload = () => {
      readFile(path, "utf8", (error, nextText) => {
        if (error) {
          if (error.code === "ENOENT") {
            nextText = "";
          } else {
            warn(ctx, `soul 文件重载失败: ${path} (${error.message})`);
            return;
          }
        }
        if (nextText === soulText) return;
        soulText = nextText;
        try {
          ctx.emit("system-prompt/change");
        } catch (emitError) {
          warn(ctx, `system prompt 更新通知失败: ${emitError instanceof Error ? emitError.message : String(emitError)}`);
        }
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
      watcher.on("error", (error) => warn(ctx, `soul 文件监听失败: ${error.message}`));
    } catch (error) {
      warn(ctx, `soul 文件监听无法启动: ${path} (${error instanceof Error ? error.message : String(error)})`);
    }

    return () => {
      if (reloadTimer !== undefined) clearTimeout(reloadTimer);
      watcher?.close();
      removeSection();
    };
  }, "riko-dsh.twin-soul");
}
