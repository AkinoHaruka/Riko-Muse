import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { apply } from "../dist/mobile-api.js";

const bridgeToken = "riko-mobile-bridge-test-token-0123456789";

test("mobile model settings follow DSH settings and write-only credential flows", async () => {
  const directory = await mkdtemp(join(tmpdir(), "riko-mobile-model-settings-"));
  const tokenPath = join(directory, "bridge-token");
  const registryPath = join(directory, "session-registry.json");
  await writeFile(tokenPath, bridgeToken, { mode: 0o600 });

  const state = {
    namespace: {
      ns: "llm-pi-ai",
      revision: 1,
      value: {
        providers: {
          openai: {
            api: "openai-completions",
            baseURL: "https://api.openai.com/v1",
            models: [{ id: "gpt-test" }],
          },
        },
      },
      user: { providers: {} },
    },
    credentials: new Map(),
    failAcmeCredentialOnce: true,
    discoveredRequest: undefined,
  };

  const settings = {
    writable: true,
    describe: () => [structuredClone(state.namespace)],
    async mutate(namespace, ops, expectedRevision) {
      assert.equal(namespace, "llm-pi-ai");
      if (expectedRevision !== undefined && expectedRevision !== state.namespace.revision) {
        const error = new Error("stale settings revision");
        error.code = "SETTINGS_CONFLICT";
        throw error;
      }
      for (const op of ops) {
        for (const root of [state.namespace.value, state.namespace.user]) {
          let cursor = root;
          for (const part of op.path.slice(0, -1)) {
            cursor[part] ??= {};
            cursor = cursor[part];
          }
          const leaf = op.path.at(-1);
          if (op.op === "set") cursor[leaf] = structuredClone(op.value);
          else delete cursor[leaf];
        }
      }
      state.namespace.revision += 1;
    },
  };

  const credentials = {
    async describe(reference) { return { configured: state.credentials.has(reference), writable: true }; },
    async set(reference, value) {
      if (reference === "ACME_GATEWAY_API_KEY" && state.failAcmeCredentialOnce) {
        state.failAcmeCredentialOnce = false;
        throw new Error(`test backend failure echoed ${value}`);
      }
      state.credentials.set(reference, value);
    },
    async unset(reference) { state.credentials.delete(reference); },
  };

  const configurableProviders = () => {
    const profiles = state.namespace.value.providers ?? {};
    return Object.entries(profiles).map(([provider, profile]) => ({
      provider,
      displayName: profile.displayName ?? provider,
      settingsNs: "llm-pi-ai",
      settingsPath: ["providers", provider],
      declared: provider !== "openai",
    }));
  };
  const llm = {
    listConfigurableProviders: configurableProviders,
    listProviders: () => configurableProviders().map((entry) => ({ id: entry.provider, name: entry.displayName })),
    async discoverModels(settingsNs, request) {
      assert.equal(settingsNs, "llm-pi-ai");
      state.discoveredRequest = request;
      return [{ id: "discovered-model", name: "Discovered", contextWindow: 128000 }];
    },
  };

  let handler;
  const server = createServer((req, res) => { void handler(req, res); });
  const ctx = {
    webServer: {
      register(route) {
        assert.equal(route.path, "/riko-app-api/v1");
        handler = route.handler;
        return () => { handler = undefined; };
      },
    },
    sessionController: {},
    settings,
    credentials,
    llm,
    effect(effect) { effect(); },
    logger: { info() {}, warn() {}, error() {} },
  };

  try {
    apply(ctx, { apiTokenFile: tokenPath, sessionRegistryFile: registryPath });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    const base = `http://127.0.0.1:${address.port}/riko-app-api/v1`;
    const request = async (path, method = "GET", body) => {
      const response = await fetch(`${base}${path}`, {
        method,
        headers: {
          authorization: `Bearer ${bridgeToken}`,
          ...(body === undefined ? {} : { "content-type": "application/json" }),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      return { status: response.status, body: await response.json() };
    };

    const described = await request("/model-settings");
    assert.equal(described.status, 200);
    assert.equal(described.body.providers[0].id, "openai");
    assert.equal(JSON.stringify(described.body).includes(bridgeToken), false);

    const keyWrite = await request("/model-settings/providers/openai/credential", "POST", {
      apiKey: "fake-openai-key-never-returned",
      expectedRevision: 1,
    });
    assert.equal(keyWrite.status, 200);
    assert.equal(state.namespace.value.providers.openai.apiKeyEnv, "OPENAI_API_KEY");
    assert.equal(state.credentials.get("OPENAI_API_KEY"), "fake-openai-key-never-returned");

    const discovery = await request("/model-settings/discover", "POST", {
      baseURL: "https://gateway.example/v1",
      api: "openai-completions",
      apiKey: "fake-discovery-key-never-returned",
    });
    assert.equal(discovery.status, 200);
    assert.equal(discovery.body.models[0].id, "discovered-model");
    assert.equal(JSON.stringify(discovery.body).includes("fake-discovery-key-never-returned"), false);

    const customProvider = {
      provider: "acme-gateway",
      displayName: "Acme Gateway",
      baseURL: "https://gateway.example/v1",
      api: "openai-completions",
      expectedRevision: 2,
      apiKey: "fake-acme-key-never-returned",
      models: [{ id: "acme-chat", name: "Acme Chat", contextWindow: 64000 }],
    };
    const firstCreate = await request("/model-settings/custom-providers", "POST", customProvider);
    assert.equal(firstCreate.status, 502);
    assert.equal(JSON.stringify(firstCreate.body).includes("fake-acme-key-never-returned"), false);

    const retryCreate = await request("/model-settings/custom-providers", "POST", customProvider);
    assert.equal(retryCreate.status, 200);
    assert.equal(state.namespace.value.providers["acme-gateway"].apiKeyEnv, "ACME_GATEWAY_API_KEY");
    assert.equal(state.credentials.get("ACME_GATEWAY_API_KEY"), "fake-acme-key-never-returned");
  } finally {
    await new Promise((resolve) => server.close(resolve));
    await rm(directory, { recursive: true, force: true });
  }
});
