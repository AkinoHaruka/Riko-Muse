# Riko App API Bridge for DSH

`@riko/riko-app-api` is a standalone DeepSeek Harness Host plugin for the Riko Android app. It owns the `/riko-app-api/v1` HTTP API, Riko-App session registry, and DSH-backed model settings. It is separate from `@agent-memory/dsh-adapter`; both packages can be installed in the same DSH profile.

## Install

From this repository checkout:

```text
corepack pnpm dsh plugin --profile <profile> add ./riko-app-bridge
```

After this package is published to the repository's default branch, install it from GitHub with:

```text
corepack pnpm dsh plugin --profile <profile> add github:AkinoHaruka/Riko-Memory#path:/riko-app-bridge
```

The bundle inserts a profile-level Host plugin. It does not modify the Riko Memory adapter or its preset patch. The plugin remains disabled until both required environment variables are configured in the DSH process or its home `.env` file:

| Variable | Purpose |
|---|---|
| `RIKO_APP_API_TOKEN_FILE` | Path to a random app-only bearer token file. Keep it outside Git and the package. |
| `RIKO_APP_SESSION_REGISTRY_FILE` | Path to the persistent registry of sessions created through this bridge. |

## API and credentials

Riko-App uses `https://riko.asia/riko-app-api/v1` by default; the URL remains editable in App settings. Put the DSH listener behind the existing HTTPS reverse proxy and route `/riko-app-api/` to it while preserving the URI prefix. Keep DSH bound to loopback.

The bridge token is separate from provider API keys. It authorizes only bridge operations and is stored encrypted by Android Keystore in the app. Provider credentials stay on the DSH host and are written through DSH `credentials`; the bridge never returns credential values.

The bridge exposes health, model catalog and selection, Riko-App session list/create/history, prompt submission, live event stream, cancellation, and model settings:

- `GET /model-settings` returns provider profiles, the DSH settings revision, and credential configured/not-configured flags.
- `POST /model-settings/discover` calls DSH `llm.discoverModels` for a configured provider endpoint.
- `POST|DELETE /model-settings/providers/{id}/credential` writes or removes a provider credential.
- `POST /model-settings/custom-providers` and `PUT|DELETE /model-settings/custom-providers/{id}` manage custom OpenAI/Anthropic-compatible providers and separately stored credentials.

New sessions use the `riko` Agent preset. The registry prevents this API from addressing unrelated DSH sessions. History returns 20 messages by default and permits up to 50 per page.

Never place the token or provider API keys in the profile patch, logs, documentation, or repository.

## Development

```text
npm install
npm test
```

The package targets DSH `0.2.0-rc.1`. Local TypeScript tests exercise bridge API behavior with fixed in-process DSH service doubles; they do not establish production deployment or external model connectivity.
