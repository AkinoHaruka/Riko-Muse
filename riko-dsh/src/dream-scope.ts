import { AsyncLocalStorage } from "node:async_hooks";

export interface DreamChildBinding {
  runnerId: string;
  jobId: string;
  generation: number;
  phase: "extract" | "redecision" | "adjudicate" | "consolidate";
  parentAgentId: string;
  readToolsEnabled: boolean;
}

const dreamChildStorage = new AsyncLocalStorage<DreamChildBinding>();

export function withDreamChildBinding<T>(binding: DreamChildBinding, action: () => T): T {
  return dreamChildStorage.run(binding, action);
}

export function currentDreamChildBinding(): DreamChildBinding | undefined {
  return dreamChildStorage.getStore();
}
