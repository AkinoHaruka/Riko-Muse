import { timingSafeEqual, randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import type { IncomingMessage, ServerResponse } from "node:http";
import { supportedProtocols } from "@deepseek-ai/dsh-llm-pi-ai";

const API_PREFIX = "/riko-app-api/v1";
const PRESET_ID = "riko";
const DSH_VERSION = "0.2.0-rc.1";
const BODY_LIMIT = 1024 * 1024;
const HISTORY_DEFAULT_MAX_MESSAGES = 20;
const HISTORY_MAX_MESSAGES = 50;

export const name = "riko-app-api";
export const inject = ["webServer", "sessionController", "settings", "credentials", "llm"];

export interface Config {
  apiTokenFile: string;
  sessionRegistryFile: string;
}

interface MessageValue {
  id: string;
  role: "user" | "assistant" | "tool";
  text: string;
  seq: number;
  requestId?: string;
}

interface SessionControllerLike {
  list(request: Record<string, never>, signal: AbortSignal): Promise<{ items: SessionSummaryLike[] }>;
  create(request: { sessionId: string; agentPreset: string }): Promise<{ sessionId: string; agentPreset?: string }>;
  modelCatalog(): Promise<unknown>;
  selectModel(request: { sessionId: string; provider: string; model: string; reasoningEffort?: string }): Promise<unknown>;
  page(request: {
    address: { kind: "session"; sessionId: string };
    throughSeq: number;
    beforeSeq: number;
    maxMessages: number;
  }, signal: AbortSignal): Promise<{ records: readonly unknown[]; hasMore: boolean }>;
  prompt(request: {
    requestId: string;
    sessionId: string;
    mode: "queue" | "steer";
    content: readonly { type: "text"; text: string }[];
  }, signal: AbortSignal): Promise<unknown>;
  cancel(request: { sessionId: string }): unknown;
  follow(request: {
    address: { kind: "session"; sessionId: string };
    assistantStream?: true;
    maxMessages?: number;
  }, signal: AbortSignal): AsyncIterable<unknown>;
}

interface SettingsNamespaceLike {
  ns: string;
  value: unknown;
  user?: unknown;
  revision: number;
}

interface SettingsControllerLike {
  writable: boolean;
  describe(options?: { redactSecrets?: boolean }): SettingsNamespaceLike[];
  mutate(
    ns: string,
    ops: Array<{ op: "set"; path: string[]; value: unknown } | { op: "unset"; path: string[] }>,
    expectedRevision?: number,
  ): Promise<void>;
}

interface CredentialControllerLike {
  describe(ref: string): Promise<{ configured: boolean; writable: boolean }>;
  set(ref: string, value: string): Promise<void>;
  unset(ref: string): Promise<void>;
}

interface LlmProviderLike {
  id: string;
  name: string;
}

interface ConfigurableProviderLike {
  provider: string;
  displayName: string;
  settingsNs: string;
  settingsPath: string[];
  active?: boolean;
  declared?: boolean;
  error?: string;
}

interface LlmRegistryLike {
  listProviders(): LlmProviderLike[];
  listConfigurableProviders(): ConfigurableProviderLike[];
  discoverModels(settingsNs: string, request: {
    provider?: string;
    baseURL?: string;
    api?: string;
    apiKey?: string;
  }): Promise<unknown[]>;
}

interface SessionSummaryLike {
  sessionId: string;
  updatedAt: number;
  running: boolean;
  blank: boolean;
}

interface WebServerLike {
  register(route: {
    kind: "prefix";
    path: string;
    handler: (req: IncomingMessage, res: ServerResponse) => void | Promise<void>;
  }): () => void;
}

interface MobileContext {
  webServer: WebServerLike;
  sessionController: SessionControllerLike;
  settings: SettingsControllerLike;
  credentials: CredentialControllerLike;
  llm: LlmRegistryLike;
  effect(effect: () => void | (() => void)): void;
  logger?: { warn(message: string): void; error(message: string): void; info(message: string): void };
}

interface RegistryState {
  version: 1;
  sessions: string[];
  createRequests: Record<string, string>;
}

class SessionRegistry {
  private tail: Promise<void> = Promise.resolve();

  private constructor(private readonly path: string, private state: RegistryState) {}

  static async open(path: string): Promise<SessionRegistry> {
    let state: RegistryState;
    try {
      const raw: unknown = JSON.parse(await readFile(path, "utf8"));
      if (!isRegistryState(raw)) throw new Error("unsupported session registry shape");
      state = raw;
    } catch (error: unknown) {
      if (isNodeError(error) && error.code === "ENOENT") {
        state = { version: 1, sessions: [], createRequests: {} };
      } else {
        throw new Error(`riko-app-api: cannot read session registry: ${errorMessage(error)}`);
      }
    }
    return new SessionRegistry(path, state);
  }

  list(): readonly string[] {
    return [...this.state.sessions];
  }

  includes(sessionId: string): boolean {
    return this.state.sessions.includes(sessionId);
  }

  async reserveCreate(requestId: string): Promise<string> {
    return this.update(async () => {
      if (Object.hasOwn(this.state.createRequests, requestId)) return this.state.createRequests[requestId]!;
      const sessionId = randomUUID();
      this.state.createRequests[requestId] = sessionId;
      await this.persist();
      return sessionId;
    });
  }

  async commitCreate(sessionId: string): Promise<void> {
    await this.update(async () => {
      if (!this.state.sessions.includes(sessionId)) this.state.sessions.push(sessionId);
      await this.persist();
    });
  }

  private async update<T>(work: () => Promise<T>): Promise<T> {
    let release!: () => void;
    const previous = this.tail;
    this.tail = new Promise<void>((resolve) => { release = resolve; });
    await previous;
    try {
      return await work();
    } finally {
      release();
    }
  }

  private async persist(): Promise<void> {
    await mkdir(dirname(this.path), { recursive: true });
    const temporary = `${this.path}.${process.pid}.${randomUUID()}.tmp`;
    await writeFile(temporary, `${JSON.stringify(this.state)}\n`, { encoding: "utf8", mode: 0o600 });
    await rename(temporary, this.path);
  }
}

class ApiFault extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
  }
}

export function apply(ctx: MobileContext, rawConfig: Config): void {
  const config = validateConfig(rawConfig);
  const apiToken = readFileSync(config.apiTokenFile, "utf8").trim();
  if (apiToken.length < 32) throw new Error("riko-app-api: token file must contain at least 32 characters");
  const mobileCtx = ctx;
  const registryPromise = SessionRegistry.open(config.sessionRegistryFile).catch((error: unknown) => {
    mobileCtx.logger?.error(`riko-app-api: session registry unavailable: ${errorMessage(error)}`);
    throw new ApiFault(503, "REGISTRY_UNAVAILABLE", "Riko session registry is unavailable");
  });
  // Keep an initialization failure handled even when no request arrives to await it.
  void registryPromise.catch(() => undefined);

  mobileCtx.effect(() => mobileCtx.webServer.register({
    kind: "prefix",
    path: API_PREFIX,
    handler: async (req, res) => {
      try {
        if (!authorized(req, apiToken)) throw new ApiFault(401, "UNAUTHORIZED", "Authentication required");
        const url = new URL(req.url ?? "/", "http://riko-app.local");
        const route = url.pathname.slice(API_PREFIX.length) || "/";
        const registry = await registryPromise;
        const sessions = mobileCtx.sessionController;
        await dispatch(
          req, res, route, url, sessions, registry,
          mobileCtx.settings, mobileCtx.credentials, mobileCtx.llm,
        );
      } catch (error: unknown) {
        if (res.headersSent) {
          if (isEventStream(res)) {
            const fault = normalizeError(error);
            sendEvent(res, "error", { type: "error", code: fault.code, message: fault.message });
          }
          res.end();
          return;
        }
        const fault = normalizeError(error);
        if (!(error instanceof ApiFault)) mobileCtx.logger?.warn(`riko-app-api: ${errorMessage(error)}`);
        sendJson(res, fault.status, { ok: false, error: { code: fault.code, message: fault.message } });
      }
    },
  }));

  mobileCtx.logger?.info?.(`riko-app-api: mounted ${API_PREFIX} for the Riko preset`);
}

async function dispatch(
  req: IncomingMessage,
  res: ServerResponse,
  route: string,
  url: URL,
  sessions: SessionControllerLike,
  registry: SessionRegistry,
  settings: SettingsControllerLike,
  credentials: CredentialControllerLike,
  llm: LlmRegistryLike,
): Promise<void> {
  if (route === "/health" && req.method === "GET") {
    sendJson(res, 200, { ok: true, dshVersion: DSH_VERSION, preset: PRESET_ID });
    return;
  }
  if (route === "/models" && req.method === "GET") {
    sendJson(res, 200, await sessions.modelCatalog());
    return;
  }
  if (route === "/model-settings" && req.method === "GET") {
    sendJson(res, 200, await describeModelSettings(settings, credentials, llm));
    return;
  }
  if (route === "/model-settings/discover" && req.method === "POST") {
    const body = await readJson(req);
    const baseURL = requiredString(body.baseURL, "baseURL", 2048).trim();
    const api = requiredString(body.api, "api", 100);
    if (!supportedProtocols().includes(api)) {
      throw new ApiFault(400, "INVALID_FIELD", "不支持此模型 API 协议");
    }
    const apiKey = optionalString(body.apiKey, 8192)?.trim();
    try {
      const found = await llm.discoverModels("llm-pi-ai", {
        baseURL,
        api,
        ...(apiKey ? { apiKey } : {}),
      });
      sendJson(res, 200, { models: found.map(projectDiscoveredModel).filter(Boolean) });
    } catch {
      // Provider responses can echo request details. Never forward or log a
      // discovery error that may contain the caller's API key.
      throw new ApiFault(502, "MODEL_DISCOVERY_FAILED", "无法读取模型列表；请检查 API 地址、协议和密钥");
    }
    return;
  }
  const credentialMatch = /^\/model-settings\/providers\/([^/]+)\/credential$/.exec(route);
  if (credentialMatch && (req.method === "POST" || req.method === "DELETE")) {
    const providerId = decodePathPart(credentialMatch[1]!);
    const reference = await providerCredentialRef(providerId, settings, llm);
    if (req.method === "POST") {
      const body = await readJson(req);
      const apiKey = requiredString(body.apiKey, "apiKey", 8192).trim();
      try {
        await bindPiAiCredentialReference(
          providerId,
          reference,
          optionalBodySafeInteger(body.expectedRevision, "expectedRevision"),
          settings,
          llm,
        );
        await credentials.set(reference, apiKey);
      } catch (error: unknown) {
        if (error instanceof ApiFault) throw error;
        throw new ApiFault(502, "CREDENTIAL_WRITE_FAILED", "DSH 未能保存此提供商密钥");
      }
      sendJson(res, 200, { provider: providerId, configured: true });
    } else {
      try {
        await credentials.unset(reference);
      } catch {
        throw new ApiFault(502, "CREDENTIAL_REMOVE_FAILED", "DSH 未能移除此提供商密钥");
      }
      sendJson(res, 200, { provider: providerId, configured: false });
    }
    return;
  }
  if (route === "/model-settings/custom-providers" && req.method === "POST") {
    const body = await readJson(req);
    await writeCustomProvider(body, undefined, settings, credentials, llm, false);
    sendJson(res, 200, { saved: true });
    return;
  }
  const customProviderMatch = /^\/model-settings\/custom-providers\/([^/]+)$/.exec(route);
  if (customProviderMatch) {
    const providerId = decodePathPart(customProviderMatch[1]!);
    if (req.method === "PUT") {
      const body = await readJson(req);
      await writeCustomProvider(body, providerId, settings, credentials, llm, true);
      sendJson(res, 200, { saved: true, provider: providerId });
      return;
    }
    if (req.method === "DELETE") {
      await removeCustomProvider(providerId, settings, credentials, llm);
      sendJson(res, 200, { removed: true, provider: providerId });
      return;
    }
  }
  if (route === "/sessions" && req.method === "GET") {
    const allowed = new Set(registry.list());
    const result = await sessions.list({}, new AbortController().signal);
    sendJson(res, 200, {
      items: result.items.filter((item) => allowed.has(item.sessionId)),
    });
    return;
  }
  if (route === "/sessions" && req.method === "POST") {
    const body = await readJson(req);
    const requestId = requiredUuid(body.requestId, "requestId");
    const sessionId = await registry.reserveCreate(requestId);
    const created = await sessions.create({ sessionId, agentPreset: PRESET_ID });
    if (created.sessionId !== sessionId || created.agentPreset !== PRESET_ID) {
      throw new ApiFault(409, "PRESET_MISMATCH", "DSH did not create the requested Riko preset session");
    }
    await registry.commitCreate(sessionId);
    sendJson(res, 201, { sessionId, agentPreset: PRESET_ID });
    return;
  }

  const match = /^\/sessions\/([^/]+)(?:\/(history|events|messages|model|cancel))?$/.exec(route);
  if (!match) throw new ApiFault(404, "NOT_FOUND", "Route not found");
  const sessionId = decodePathPart(match[1]!);
  if (!registry.includes(sessionId)) throw new ApiFault(404, "NOT_FOUND", "Riko-App session not found");
  const action = match[2];

  if (action === "history" && req.method === "GET") {
    const throughSeq = optionalSafeInteger(url.searchParams.get("throughSeq"), "throughSeq");
    const beforeSeq = optionalSafeInteger(url.searchParams.get("beforeSeq"), "beforeSeq");
    const maxMessages = optionalSafeInteger(url.searchParams.get("maxMessages"), "maxMessages") ?? HISTORY_DEFAULT_MAX_MESSAGES;
    if (maxMessages < 1 || maxMessages > HISTORY_MAX_MESSAGES) {
      throw new ApiFault(400, "INVALID_FIELD", `maxMessages must be between 1 and ${HISTORY_MAX_MESSAGES}`);
    }
    if ((throughSeq === undefined) !== (beforeSeq === undefined)) {
      throw new ApiFault(400, "INVALID_FIELD", "throughSeq and beforeSeq must be provided together");
    }
    if (throughSeq !== undefined && beforeSeq !== undefined) {
      const page = await sessions.page({
        address: { kind: "session", sessionId },
        throughSeq,
        beforeSeq,
        maxMessages,
      }, new AbortController().signal);
      sendJson(res, 200, {
        sessionId,
        cursor: throughSeq,
        messages: projectRecords(page.records),
        hasMore: page.hasMore,
        nextBeforeSeq: earliestRecordSeq(page.records),
      });
      return;
    }
    const historyController = new AbortController();
    const frames = sessions.follow({
      address: { kind: "session", sessionId },
      assistantStream: true,
      maxMessages,
    }, historyController.signal);
    const iterator = frames[Symbol.asyncIterator]();
    try {
      const first = await iterator.next();
      if (first.done || !isSnapshot(first.value)) throw new ApiFault(404, "NOT_FOUND", "Session history unavailable");
      sendJson(res, 200, {
        sessionId,
        cursor: first.value.cursor,
        messages: projectRecords(first.value.records),
        hasMore: first.value.hasMore,
        nextBeforeSeq: earliestRecordSeq(first.value.records),
      });
    } finally {
      historyController.abort();
      await iterator.return?.();
    }
    return;
  }
  if (action === "events" && req.method === "GET") {
    await streamSession(req, res, sessionId, sessions);
    return;
  }
  if (action === "messages" && req.method === "POST") {
    const body = await readJson(req);
    const requestId = requiredUuid(body.requestId, "requestId");
    const text = requiredString(body.text, "text", 100_000);
    if (body.mode !== undefined && body.mode !== "queue" && body.mode !== "steer") {
      throw new ApiFault(400, "INVALID_FIELD", "mode must be queue or steer");
    }
    const mode = body.mode === "steer" ? "steer" : "queue";
    await sessions.prompt({
      requestId,
      sessionId,
      mode,
      content: [{ type: "text", text }],
    }, new AbortController().signal);
    sendJson(res, 202, { accepted: true, requestId });
    return;
  }
  if (action === "model" && req.method === "POST") {
    const body = await readJson(req);
    const provider = requiredString(body.provider, "provider", 200);
    const model = requiredString(body.model, "model", 300);
    const reasoningEffort = optionalString(body.reasoningEffort, 100);
    const selected = await sessions.selectModel({
      sessionId,
      provider,
      model,
      ...(reasoningEffort ? { reasoningEffort } : {}),
    });
    sendJson(res, 200, selected);
    return;
  }
  if (action === "cancel" && req.method === "POST") {
    await sessions.cancel({ sessionId });
    sendJson(res, 200, { accepted: true });
    return;
  }
  throw new ApiFault(405, "METHOD_NOT_ALLOWED", "Method not allowed for this route");
}

async function bindPiAiCredentialReference(
  providerId: string,
  reference: string,
  requestedRevision: number | undefined,
  settings: SettingsControllerLike,
  llm: LlmRegistryLike,
): Promise<void> {
  const entry = llm.listConfigurableProviders().find((item) => item.provider === providerId);
  if (!entry || entry.settingsNs !== "llm-pi-ai" || entry.settingsPath.length === 0) return;
  const namespace = settings.describe({ redactSecrets: true }).find((item) => item.ns === entry.settingsNs);
  if (!namespace) throw new ApiFault(503, "MODEL_SETTINGS_UNAVAILABLE", "DSH 模型设置当前不可用");
  const resolved = getPath(namespace.value, entry.settingsPath);
  const currentRef = isObject(resolved) && typeof resolved.apiKeyEnv === "string"
    ? resolved.apiKeyEnv
    : undefined;
  if (currentRef !== undefined) return;
  try {
    // This mirrors DSH ProviderEditor: attach the derived reference to the
    // profile before storing the credential. Path mutation creates missing
    // intermediate objects and leaves every other profile field untouched.
    await settings.mutate(
      entry.settingsNs,
      [{ op: "set", path: [...entry.settingsPath, "apiKeyEnv"], value: reference }],
      requestedRevision ?? namespace.revision,
    );
  } catch (error: unknown) {
    if (isSettingsConflict(error)) {
      throw new ApiFault(409, "SETTINGS_CONFLICT", "模型设置已被其他窗口修改，请刷新后重试");
    }
    throw new ApiFault(422, "MODEL_SETTINGS_REJECTED", "DSH 拒绝了 API Key 凭据引用");
  }
}

async function describeModelSettings(
  settings: SettingsControllerLike,
  credentials: CredentialControllerLike,
  llm: LlmRegistryLike,
): Promise<Record<string, unknown>> {
  const namespaces = settings.describe({ redactSecrets: true });
  const byNamespace = new Map(namespaces.map((item) => [item.ns, item]));
  const directory = llm.listConfigurableProviders();
  const registered = llm.listProviders();
  const known = new Set(directory.map((item) => item.provider));
  const entries: ConfigurableProviderLike[] = [
    ...directory,
    ...registered.filter((item) => !known.has(item.id)).map((item) => ({
      provider: item.id,
      displayName: item.name,
      settingsNs: "",
      settingsPath: [],
    })),
  ];
  const providers = await Promise.all(entries.map(async (entry) => {
    const namespace = byNamespace.get(entry.settingsNs);
    const profile = namespace ? getPath(namespace.value, entry.settingsPath) : undefined;
    const view = projectProviderProfile(profile);
    const reference = entry.provider === "deepseek-account"
      ? undefined
      : validCredentialRef(view.apiKeyEnv) ? view.apiKeyEnv : deriveCredentialRef(entry.provider);
    let keyConfigured: boolean | null = null;
    if (reference) {
      try {
        keyConfigured = (await credentials.describe(reference)).configured;
      } catch {
        // Credential status is a badge; a read failure must not expose any
        // provider or credential backend diagnostic to the mobile client.
      }
    }
    return {
      id: entry.provider,
      name: view.displayName ?? entry.displayName,
      active: registered.some((item) => item.id === entry.provider),
      configurable: Boolean(entry.settingsNs),
      custom: entry.settingsNs === "llm-pi-ai" && entry.declared === true,
      configured: profile !== undefined,
      credentialRef: reference,
      keyConfigured,
      profile: view,
      error: entry.error,
    };
  }));
  const piNamespace = byNamespace.get("llm-pi-ai");
  return {
    writable: settings.writable,
    revision: piNamespace?.revision ?? 0,
    protocols: supportedProtocols(),
    providers,
  };
}

function projectProviderProfile(value: unknown): {
  displayName?: string;
  apiKeyEnv?: string;
  api?: string;
  baseURL?: string;
  models: Array<Record<string, unknown>>;
} {
  if (!isObject(value)) return { models: [] };
  const models = Array.isArray(value.models)
    ? value.models.flatMap((item) => {
      if (!isObject(item) || typeof item.id !== "string") return [];
      return [{
        id: item.id,
        ...(typeof item.name === "string" ? { name: item.name } : {}),
        ...(isPositiveInteger(item.contextWindow) ? { contextWindow: item.contextWindow } : {}),
        ...(isPositiveInteger(item.maxTokens) ? { maxTokens: item.maxTokens } : {}),
      }];
    })
    : [];
  return {
    ...(typeof value.displayName === "string" ? { displayName: value.displayName } : {}),
    ...(typeof value.apiKeyEnv === "string" ? { apiKeyEnv: value.apiKeyEnv } : {}),
    ...(typeof value.api === "string" ? { api: value.api } : {}),
    ...(typeof value.baseURL === "string" ? { baseURL: value.baseURL } : {}),
    models,
  };
}

function projectDiscoveredModel(value: unknown): Record<string, unknown> | undefined {
  if (!isObject(value) || typeof value.id !== "string" || value.id.length === 0) return undefined;
  return {
    id: value.id,
    ...(typeof value.name === "string" ? { name: value.name } : {}),
    ...(isPositiveInteger(value.contextWindow) ? { contextWindow: value.contextWindow } : {}),
    ...(isPositiveInteger(value.maxTokens) ? { maxTokens: value.maxTokens } : {}),
  };
}

async function providerCredentialRef(
  providerId: string,
  settings: SettingsControllerLike,
  llm: LlmRegistryLike,
): Promise<string> {
  if (providerId === "deepseek-account") {
    throw new ApiFault(409, "SPECIAL_PROVIDER_AUTH", "此提供商使用 DSH 专用登录方式");
  }
  const entry = llm.listConfigurableProviders().find((item) => item.provider === providerId);
  if (!entry && !llm.listProviders().some((item) => item.id === providerId)) {
    throw new ApiFault(404, "PROVIDER_NOT_FOUND", "DSH 中不存在此模型提供商");
  }
  if (entry) {
    const namespace = settings.describe({ redactSecrets: true }).find((item) => item.ns === entry.settingsNs);
    const profile = namespace ? getPath(namespace.value, entry.settingsPath) : undefined;
    const ref = isObject(profile) && typeof profile.apiKeyEnv === "string"
      ? profile.apiKeyEnv
      : deriveCredentialRef(providerId);
    if (validCredentialRef(ref)) return ref;
  }
  const derived = deriveCredentialRef(providerId);
  if (!validCredentialRef(derived)) throw new ApiFault(409, "CREDENTIAL_REF_UNAVAILABLE", "此提供商没有可用的凭据引用");
  return derived;
}

async function writeCustomProvider(
  body: Record<string, unknown>,
  pathProviderId: string | undefined,
  settings: SettingsControllerLike,
  credentials: CredentialControllerLike,
  llm: LlmRegistryLike,
  updating: boolean,
): Promise<void> {
  const providerId = pathProviderId ?? requiredString(body.provider, "provider", 100).trim();
  if (!/^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/.test(providerId)) {
    throw new ApiFault(400, "INVALID_FIELD", "自定义提供商 ID 格式无效");
  }
  const baseURL = requiredString(body.baseURL, "baseURL", 2048).trim();
  let parsedURL: URL;
  try { parsedURL = new URL(baseURL); } catch {
    throw new ApiFault(400, "INVALID_FIELD", "API 地址格式无效");
  }
  if ((parsedURL.protocol !== "http:" && parsedURL.protocol !== "https:")
    || parsedURL.username.length > 0 || parsedURL.password.length > 0) {
    throw new ApiFault(400, "INVALID_FIELD", "API 地址必须是 HTTP(S) URL，且不能嵌入账号或密钥");
  }
  const api = requiredString(body.api, "api", 100);
  if (!supportedProtocols().includes(api)) throw new ApiFault(400, "INVALID_FIELD", "不支持此模型 API 协议");
  const models = parseProviderModels(body.models);
  const displayName = optionalString(body.displayName, 200)?.trim();
  const apiKey = optionalString(body.apiKey, 8192)?.trim();
  const ns = settings.describe({ redactSecrets: true }).find((item) => item.ns === "llm-pi-ai");
  if (!ns) throw new ApiFault(503, "MODEL_SETTINGS_UNAVAILABLE", "DSH 自定义模型设置当前不可用");
  const current = getPath(ns.value, ["providers", providerId]);
  if (updating && !isObject(current)) throw new ApiFault(404, "PROVIDER_NOT_FOUND", "自定义提供商不存在");
  const existing = isObject(current) ? current : {};
  const routeTakenOutsideCustom = llm.listConfigurableProviders().some((item) => item.provider === providerId
    && !(item.settingsNs === "llm-pi-ai" && item.declared === true))
    || llm.listProviders().some((item) => item.id === providerId) && current === undefined;
  if (!updating && routeTakenOutsideCustom) {
    throw new ApiFault(409, "PROVIDER_EXISTS", "此提供商 ID 已被 DSH 使用");
  }
  const credentialRef = apiKey
    ? deriveCredentialRef(providerId)
    : (typeof existing.apiKeyEnv === "string" ? existing.apiKeyEnv : undefined);
  if (credentialRef !== undefined && !validCredentialRef(credentialRef)) {
    throw new ApiFault(409, "CREDENTIAL_REF_UNAVAILABLE", "DSH 当前凭据引用格式不可用");
  }
  const profile = {
    ...existing,
    ...(displayName ? { displayName } : {}),
    ...(credentialRef ? { apiKeyEnv: credentialRef } : {}),
    api,
    baseURL,
    models,
  };
  // A create may commit the profile and then fail while storing the key. Let
  // the same POST retry finish that write-only credential operation without
  // either overwriting a different route or requiring the stale old revision.
  if (!updating && current !== undefined && !sameManagedProviderProfile(existing, profile)) {
    throw new ApiFault(409, "PROVIDER_EXISTS", "此提供商 ID 已被 DSH 使用");
  }
  const expectedRevision = optionalBodySafeInteger(body.expectedRevision, "expectedRevision") ?? ns.revision;
  if (!sameManagedProviderProfile(existing, profile)) {
    try {
      await settings.mutate("llm-pi-ai", [{ op: "set", path: ["providers", providerId], value: profile }], expectedRevision);
    } catch (error: unknown) {
      if (isSettingsConflict(error)) throw new ApiFault(409, "SETTINGS_CONFLICT", "模型设置已被其他窗口修改，请刷新后重试");
      throw new ApiFault(422, "MODEL_SETTINGS_REJECTED", "DSH 拒绝了这组提供商设置，请检查协议、地址和模型 ID");
    }
  }
  if (apiKey && credentialRef) {
    try {
      await credentials.set(credentialRef, apiKey);
    } catch {
      // The profile is committed first, matching DSH's Models editor. A retry
      // is safe: same-profile requests skip the stale-revision write and retry
      // only this write-only credential operation.
      throw new ApiFault(502, "CREDENTIAL_WRITE_FAILED", "提供商设置已保存，但 API Key 未写入 DSH；请重试保存密钥");
    }
  }
}

async function removeCustomProvider(
  providerId: string,
  settings: SettingsControllerLike,
  credentials: CredentialControllerLike,
  llm: LlmRegistryLike,
): Promise<void> {
  if (!/^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/.test(providerId)) {
    throw new ApiFault(400, "INVALID_FIELD", "自定义提供商 ID 格式无效");
  }
  const entry = llm.listConfigurableProviders().find((item) => item.provider === providerId
    && item.settingsNs === "llm-pi-ai" && item.declared === true);
  if (!entry) throw new ApiFault(404, "PROVIDER_NOT_FOUND", "自定义提供商不存在");
  const ns = settings.describe({ redactSecrets: true }).find((item) => item.ns === "llm-pi-ai");
  if (!ns) throw new ApiFault(503, "MODEL_SETTINGS_UNAVAILABLE", "DSH 自定义模型设置当前不可用");
  const profile = getPath(ns.value, ["providers", providerId]);
  const reference = isObject(profile) && typeof profile.apiKeyEnv === "string"
    ? profile.apiKeyEnv
    : deriveCredentialRef(providerId);
  try {
    await settings.mutate("llm-pi-ai", [{ op: "unset", path: ["providers", providerId] }], ns.revision);
  } catch (error: unknown) {
    if (isSettingsConflict(error)) throw new ApiFault(409, "SETTINGS_CONFLICT", "模型设置已被其他窗口修改，请刷新后重试");
    throw new ApiFault(422, "MODEL_SETTINGS_REJECTED", "DSH 未能移除此自定义提供商");
  }
  if (validCredentialRef(reference)) {
    try {
      await credentials.unset(reference);
    } catch {
      throw new ApiFault(502, "CREDENTIAL_REMOVE_FAILED", "提供商已移除，但 DSH 未能清除它的 API Key");
    }
  }
}

function parseProviderModels(value: unknown): Array<Record<string, unknown>> {
  if (!Array.isArray(value) || value.length === 0 || value.length > 100) {
    throw new ApiFault(400, "INVALID_FIELD", "至少需要一个模型，最多支持 100 个");
  }
  const seen = new Set<string>();
  return value.map((item) => {
    const model = typeof item === "string" ? { id: item } : item;
    if (!isObject(model)) throw new ApiFault(400, "INVALID_FIELD", "模型条目格式无效");
    const id = requiredString(model.id, "model.id", 300).trim();
    if (seen.has(id)) throw new ApiFault(400, "INVALID_FIELD", "模型 ID 不能重复");
    seen.add(id);
    const name = optionalString(model.name, 300)?.trim();
    const contextWindow = optionalBodySafeInteger(model.contextWindow, "model.contextWindow");
    const maxTokens = optionalBodySafeInteger(model.maxTokens, "model.maxTokens");
    return {
      id,
      ...(name ? { name } : {}),
      ...(contextWindow === undefined ? {} : { contextWindow }),
      ...(maxTokens === undefined ? {} : { maxTokens }),
    };
  });
}

function sameManagedProviderProfile(current: Record<string, unknown>, next: Record<string, unknown>): boolean {
  const fields = ["api", "baseURL", "apiKeyEnv"] as const;
  if (fields.some((field) => current[field] !== next[field])) return false;
  if (next.displayName !== undefined && current.displayName !== next.displayName) return false;
  const modelIds = (value: unknown): unknown => Array.isArray(value)
    ? value.map((model) => isObject(model) ? [model.id, model.name, model.contextWindow, model.maxTokens] : model)
    : value;
  return JSON.stringify(modelIds(current.models)) === JSON.stringify(modelIds(next.models));
}

function deriveCredentialRef(providerId: string): string {
  return `${providerId.toUpperCase().replace(/[^A-Z0-9]+/g, "_")}_API_KEY`;
}

function validCredentialRef(value: unknown): value is string {
  return typeof value === "string" && /^[A-Za-z_][A-Za-z0-9_]*$/.test(value);
}

function getPath(value: unknown, path: readonly string[]): unknown {
  let current = value;
  for (const part of path) {
    if (!isObject(current)) return undefined;
    current = current[part];
  }
  return current;
}

function optionalBodySafeInteger(value: unknown, field: string): number | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new ApiFault(400, "INVALID_FIELD", `${field} must be a non-negative integer`);
  }
  return value;
}

function isPositiveInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function isSettingsConflict(error: unknown): boolean {
  return isObject(error) && Reflect.get(error, "code") === "SETTINGS_CONFLICT";
}

async function streamSession(
  req: IncomingMessage,
  res: ServerResponse,
  sessionId: string,
  sessions: SessionControllerLike,
): Promise<void> {
  res.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
    "x-accel-buffering": "no",
  });
  res.flushHeaders?.();
  const controller = new AbortController();
  const stop = (): void => controller.abort();
  req.once("aborted", stop);
  res.once("close", stop);
  const heartbeat = setInterval(() => {
    if (!res.writableEnded && !res.destroyed) res.write(": keep-alive\n\n");
  }, 15_000);
  try {
    const frames = sessions.follow({ address: { kind: "session", sessionId }, assistantStream: true }, controller.signal);
    for await (const frame of frames) {
      if (controller.signal.aborted || res.destroyed) break;
      for (const event of projectFrame(frame, sessionId)) sendEvent(res, "update", event);
    }
  } finally {
    clearInterval(heartbeat);
    req.off("aborted", stop);
    res.off("close", stop);
    if (!res.writableEnded) res.end();
  }
}

function projectFrame(value: unknown, sessionId: string): Record<string, unknown>[] {
  if (!isObject(value)) return [];
  if (value.type === "snapshot") {
    const messages = Array.isArray(value.records) ? projectRecords(value.records) : [];
    const out: Record<string, unknown>[] = [{ type: "snapshot", sessionId, cursor: numberOrZero(value.cursor), messages }];
    const active = isObject(value.assistantStream) && isObject(value.assistantStream.activeAttempt)
      ? value.assistantStream.activeAttempt
      : undefined;
    if (active !== undefined) {
      const text = compactStreamText(active.stream);
      out.push({ type: "activity", kind: "thinking", active: true });
      if (text) out.push({ type: "delta", attemptId: stringOrEmpty(active.attemptId), text });
    }
    return out;
  }
  if (value.type === "event" && isObject(value.event)) {
    const event = value.event;
    const message = projectEvent(event);
    if (message) return [{ type: "message", sessionId, message }];
    if (event.type === "turn/start") return [{ type: "activity", kind: "thinking", active: true }];
    if (event.type === "turn/end") return [{ type: "activity", kind: "thinking", active: false }];
    if (event.type === "tool/call" && isObject(event.data)) {
      return [{ type: "activity", kind: "tool", active: true, label: stringOrEmpty(event.data.name) }];
    }
    if (event.type === "tool/result") return [{ type: "activity", kind: "tool", active: false }];
    return [];
  }
  if (value.type === "assistant-stream" && isObject(value.frame)) {
    const frame = value.frame;
    if (frame.type === "start") return [{ type: "activity", kind: "thinking", active: true }];
    if (frame.type === "chunk" && isObject(frame.chunk) && frame.chunk.type === "text-delta") {
      return [{ type: "delta", attemptId: stringOrEmpty(frame.attemptId), text: stringOrEmpty(frame.chunk.text) }];
    }
    if (frame.type === "end" && isObject(frame.outcome) && frame.outcome.kind === "abandoned") {
      return [{ type: "activity", kind: "thinking", active: false }];
    }
  }
  return [];
}

function projectRecords(records: readonly unknown[]): MessageValue[] {
  return records.flatMap((record) => {
    if (!isObject(record) || record.type !== "event" || !isObject(record.event)) return [];
    const message = projectEvent(record.event);
    return message ? [message] : [];
  });
}

function projectEvent(event: Record<string, unknown>): MessageValue | undefined {
  if (!isObject(event.data)) return undefined;
  const data = event.data;
  let role: MessageValue["role"];
  let message: Record<string, unknown>;
  if (event.type === "user/message") {
    role = "user";
    message = data;
  } else if (event.type === "assistant/message" || event.type === "tool/result") {
    role = event.type === "assistant/message" ? "assistant" : "tool";
    if (!isObject(data.message)) return undefined;
    message = data.message;
  } else {
    return undefined;
  }
  const source = isObject(message.source) ? message.source : undefined;
  const requestId = typeof source?.rpcId === "string" ? source.rpcId : undefined;
  return {
    id: stringOrEmpty(message.id) || `${String(event.type)}:${String(event.seq)}`,
    role,
    text: contentText(message.content),
    seq: numberOrZero(event.seq),
    ...(requestId ? { requestId } : {}),
  };
}

function contentText(value: unknown): string {
  if (!Array.isArray(value)) return typeof value === "string" ? value : "";
  return value.map((block) => {
    if (!isObject(block)) return "";
    if (block.type === "text") return stringOrEmpty(block.text);
    if (block.type === "image") return "[图片]";
    if (block.type === "file") return "[文件]";
    return "";
  }).filter(Boolean).join("");
}

function compactStreamText(value: unknown): string {
  if (!Array.isArray(value)) return "";
  return value.map((record) => {
    if (!isObject(record)) return "";
    if (record.type === "text-chunks" && Array.isArray(record.texts)) {
      return record.texts.filter((part): part is string => typeof part === "string").join("");
    }
    if (record.type === "chunk" && isObject(record.chunk) && record.chunk.type === "text-delta") {
      return stringOrEmpty(record.chunk.text);
    }
    return "";
  }).join("");
}

function authorized(req: IncomingMessage, expected: string): boolean {
  const header = req.headers.authorization;
  if (typeof header !== "string" || !header.startsWith("Bearer ")) return false;
  const supplied = Buffer.from(header.slice(7));
  const secret = Buffer.from(expected);
  return supplied.length === secret.length && timingSafeEqual(supplied, secret);
}

async function readJson(req: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const part of req) {
    const chunk = Buffer.isBuffer(part) ? part : Buffer.from(part);
    size += chunk.length;
    if (size > BODY_LIMIT) throw new ApiFault(413, "BODY_TOO_LARGE", "Request body is too large");
    chunks.push(chunk);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new ApiFault(400, "INVALID_JSON", "Request body must be valid JSON");
  }
  if (!isObject(parsed)) throw new ApiFault(400, "INVALID_BODY", "Request body must be a JSON object");
  return parsed;
}

function requiredString(value: unknown, field: string, maxLength: number): string {
  if (typeof value !== "string" || value.trim().length === 0 || value.length > maxLength) {
    throw new ApiFault(400, "INVALID_FIELD", `${field} is required and must be within the length limit`);
  }
  return value;
}

function requiredUuid(value: unknown, field: string): string {
  const text = requiredString(value, field, 36);
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(text)) {
    throw new ApiFault(400, "INVALID_FIELD", `${field} must be a UUID`);
  }
  return text;
}

function optionalSafeInteger(value: string | null, field: string): number | undefined {
  if (value === null) return undefined;
  if (!/^(0|[1-9][0-9]*)$/.test(value)) throw new ApiFault(400, "INVALID_FIELD", `${field} must be a non-negative integer`);
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed)) throw new ApiFault(400, "INVALID_FIELD", `${field} is outside the supported range`);
  return parsed;
}

function optionalString(value: unknown, maxLength: number): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string" || value.length > maxLength) throw new ApiFault(400, "INVALID_FIELD", "Invalid optional string field");
  return value;
}

function decodePathPart(value: string): string {
  try {
    const decoded = decodeURIComponent(value);
    if (decoded.length === 0 || decoded.length > 200 || decoded.includes("/") || decoded.includes("\\")) {
      throw new Error("invalid session id");
    }
    return decoded;
  } catch {
    throw new ApiFault(400, "INVALID_SESSION_ID", "Invalid session id");
  }
}

function validateConfig(value: Config): Config {
  if (!value || typeof value.apiTokenFile !== "string" || !value.apiTokenFile.trim()
    || typeof value.sessionRegistryFile !== "string" || !value.sessionRegistryFile.trim()) {
    throw new Error("riko-app-api: apiTokenFile and sessionRegistryFile are required");
  }
  return { apiTokenFile: value.apiTokenFile.trim(), sessionRegistryFile: value.sessionRegistryFile.trim() };
}

function sendJson(res: ServerResponse, status: number, value: unknown): void {
  const body = JSON.stringify(value);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
    "content-length": Buffer.byteLength(body),
  });
  res.end(body);
}

function sendEvent(res: ServerResponse, event: string, value: unknown): void {
  if (res.writableEnded || res.destroyed) return;
  res.write(`event: ${event}\ndata: ${JSON.stringify(value)}\n\n`);
}

function isEventStream(res: ServerResponse): boolean {
  const contentType = res.getHeader("content-type");
  return typeof contentType === "string" && contentType.includes("text/event-stream");
}

function isSnapshot(value: unknown): value is { type: "snapshot"; cursor: number; records: readonly unknown[]; hasMore: boolean } {
  return isObject(value) && value.type === "snapshot" && typeof value.cursor === "number"
    && Array.isArray(value.records) && typeof value.hasMore === "boolean";
}

function earliestRecordSeq(records: readonly unknown[]): number | null {
  const values = records.flatMap((record) => {
    if (!isObject(record) || !isObject(record.event)) return [];
    return typeof record.event.seq === "number" && Number.isSafeInteger(record.event.seq) ? [record.event.seq] : [];
  });
  return values.length === 0 ? null : Math.min(...values);
}

function isRegistryState(value: unknown): value is RegistryState {
  return isObject(value) && value.version === 1 && Array.isArray(value.sessions)
    && value.sessions.every((item) => typeof item === "string") && isObject(value.createRequests)
    && Object.values(value.createRequests).every((item) => typeof item === "string");
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function numberOrZero(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function stringOrEmpty(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function normalizeError(error: unknown): ApiFault {
  if (error instanceof ApiFault) return error;
  if (isObject(error) && error.isDSHRemoteError === true && typeof error.code === "string") {
    const status = remoteStatus(error.code);
    if (status !== undefined) {
      const message = error instanceof Error ? error.message : "DSH request failed";
      return new ApiFault(status, error.code, message);
    }
  }
  return new ApiFault(500, "INTERNAL", "Riko API request failed");
}

function remoteStatus(code: string): number | undefined {
  if (code === "gateway/bad-request" || code === "session/attachment-invalid") return 400;
  if (code === "session/not-found" || code === "subagent/not-found") return 404;
  if (code === "session/model-unavailable" || code === "session/provider-credentials-unavailable"
    || code === "session/provider-models-unavailable") return 422;
  if (code === "session/agent-busy" || code === "session/writer-held" || code === "session/conflict"
    || code === "agent-preset/conflict" || code === "session/steer-unavailable"
    || code === "session/queue-item-not-found") return 409;
  if (code === "gateway/cancelled") return 409;
  return undefined;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isNodeError(error: unknown): error is NodeJS.ErrnoException {
  return error instanceof Error && "code" in error;
}
