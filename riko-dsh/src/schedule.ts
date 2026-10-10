/** Plugin-local task scheduler (H8).
 *
 * DSH does not expose a native schedule API as of the checked version, so this
 * module implements a minimal in-plugin scheduler using Node.js timers.
 *
 * Limitations (documented honestly):
 * - Tasks are in-memory only; they do NOT survive plugin reload/restart.
 * - For persistent scheduling, users should use the host's native cron/systemd.
 * - When DSH adds a native schedule API, this module should delegate to it.
 *
 * API mirrors Muse's cron interface: create / list / update / delete.
 */
import type { Context } from "@deepseek-ai/cordis";

export type ScheduleKind = "once" | "daily" | "weekly" | "interval";

export interface ScheduleTask {
  id: string;
  kind: ScheduleKind;
  /** For 'once': delay in ms. For 'interval': interval in ms. For daily/weekly: time spec. */
  spec: string | number;
  /** Human-readable description. */
  description: string;
  /** Callback invoked when the task fires. */
  handler: () => void | Promise<void>;
  createdAt: number;
}

interface ActiveTask extends ScheduleTask {
  timer: ReturnType<typeof setTimeout> | ReturnType<typeof setInterval>;
}

const tasks = new Map<string, ActiveTask>();
let nextId = 1;

function warn(ctx: Context | undefined, message: string): void {
  const logger = (ctx as unknown as { logger?: { warn?: (text: string) => void } } | undefined)?.logger;
  logger?.warn?.(`riko-dsh: ${message}`);
}

/** Parse a simple time spec. Supports: "HH:MM" (daily), number (ms). */
function msUntil(spec: string | number, kind: ScheduleKind): number {
  if (typeof spec === "number") return Math.max(0, spec);
  // "HH:MM" -> ms until next occurrence today (or tomorrow)
  const m = /^(\d{1,2}):(\d{2})$/.exec(spec.trim());
  if (m && m[1] !== undefined && m[2] !== undefined && (kind === "daily" || kind === "weekly")) {
    const now = new Date();
    const target = new Date(now);
    target.setHours(parseInt(m[1], 10), parseInt(m[2], 10), 0, 0);
    let diff = target.getTime() - now.getTime();
    if (diff <= 0) diff += 24 * 60 * 60 * 1000;
    return diff;
  }
  throw new Error(`riko-dsh: unsupported schedule spec: ${spec}`);
}

export function scheduleCreate(
  ctx: Context | undefined,
  task: Omit<ScheduleTask, "id" | "createdAt">,
): string {
  const id = `sched-${nextId++}-${Date.now().toString(36)}`;
  const full: ScheduleTask = { ...task, id, createdAt: Date.now() };

  const fire = () => {
    try {
      const r = full.handler();
      if (r instanceof Promise) r.catch((e) => warn(ctx, `scheduled task ${id} failed: ${String(e)}`));
    } catch (e) {
      warn(ctx, `scheduled task ${id} threw: ${String(e)}`);
    }
  };

  let timer: ReturnType<typeof setTimeout> | ReturnType<typeof setInterval>;
  if (full.kind === "once" || full.kind === "daily" || full.kind === "weekly") {
    const delay = msUntil(full.spec, full.kind);
    if (full.kind === "once") {
      timer = setTimeout(() => {
        fire();
        tasks.delete(id);
      }, delay);
    } else {
      // Daily/weekly: fire once at target time, then reschedule every 24h/7d.
      const period = full.kind === "daily" ? 24 * 60 * 60 * 1000 : 7 * 24 * 60 * 60 * 1000;
      timer = setTimeout(() => {
        fire();
        const active = tasks.get(id);
        if (active) {
          active.timer = setInterval(fire, period);
        }
      }, delay);
    }
  } else {
    // interval
    const intervalMs = msUntil(full.spec, full.kind);
    timer = setInterval(fire, intervalMs);
  }

  tasks.set(id, { ...full, timer });
  return id;
}

export function scheduleList(): Array<Omit<ScheduleTask, "handler">> {
  return [...tasks.values()].map(({ handler: _h, timer: _t, ...rest }) => rest);
}

export function scheduleDelete(id: string): boolean {
  const active = tasks.get(id);
  if (!active) return false;
  clearTimeout(active.timer as ReturnType<typeof setTimeout>);
  clearInterval(active.timer as ReturnType<typeof setInterval>);
  tasks.delete(id);
  return true;
}

export function scheduleUpdate(
  id: string,
  updates: Partial<Pick<ScheduleTask, "description" | "spec">>,
): boolean {
  const active = tasks.get(id);
  if (!active) return false;
  // Simplest correct semantics: delete + recreate with same handler.
  const { handler, kind, description, spec } = active;
  scheduleDelete(id);
  const newId = scheduleCreate(undefined, {
    kind,
    spec: updates.spec ?? spec,
    description: updates.description ?? description,
    handler,
  });
  // Keep the original id stable by renaming.
  const recreated = tasks.get(newId);
  if (recreated) {
    tasks.delete(newId);
    tasks.set(id, { ...recreated, id });
  }
  return true;
}

/** Clear all tasks (for tests / plugin unload). */
export function scheduleClearAll(): void {
  for (const id of [...tasks.keys()]) scheduleDelete(id);
}
