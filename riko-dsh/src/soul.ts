/**
 * File-based twin personality.
 *
 * B1 编排收口：内核 Soul（order 10300，按 agent 隔离）是单一真相源。
 * 本模块提供文件监听与读取辅助（createFileSoulWatcher）；旧版全局平行注入
 * registerTwinSoul 已废弃，默认不再调用。
 */
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

export interface FileSoulWatcher {
  getText(): string;
  close(): void;
}

/** Create a hot-reloading watcher for a local soul file. */
export function createFileSoulWatcher(
  soulFile = defaultSoulFilePath(),
  onChange?: () => void,
  logger?: { warn(msg: string): void },
): FileSoulWatcher {
  const path = resolve(soulFile);
  let soulText = "";
  try {
    soulText = readFileSync(path, "utf8");
  } catch (error) {
    logger?.warn(`soul 文件无法读取: ${path} (${error instanceof Error ? error.message : String(error)})`);
  }

  let watcher: FSWatcher | undefined;
  let reloadTimer: ReturnType<typeof setTimeout> | undefined;

  const reload = () => {
    readFile(path, "utf8", (error, nextText) => {
      if (error) {
        if (error.code === "ENOENT") {
          nextText = "";
        } else {
          logger?.warn(`soul 文件重载失败: ${path} (${error.message})`);
          return;
        }
      }
      if (nextText === soulText) return;
      soulText = nextText;
      try {
        onChange?.();
      } catch (emitError) {
        logger?.warn(`system prompt 更新通知失败: ${emitError instanceof Error ? emitError.message : String(emitError)}`);
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
    watcher.on("error", (error) => logger?.warn(`soul 文件监听失败: ${error.message}`));
  } catch (error) {
    logger?.warn(`soul 文件监听无法启动: ${path} (${error instanceof Error ? error.message : String(error)})`);
  }

  return {
    getText: () => soulText,
    close: () => {
      if (reloadTimer !== undefined) clearTimeout(reloadTimer);
      watcher?.close();
    },
  };
}

/**
 * @deprecated B1 编排收口：默认只走内核 Soul（order 10300）；
 * 全局 twin-soul（order 100）平行注入已废弃。若需文件覆盖，使用 config.fileSoulEnabled opt-in。
 */
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
