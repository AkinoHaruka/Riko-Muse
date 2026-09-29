# Riko Memory for DSH

This directory is both the DSH adapter package and an installable DSH bundle. The bundle adds a separate **Riko** Agent preset built from Minimal's persona and terminal entries, with Riko Memory appended. The shipped Minimal preset remains unchanged.

## Install

Install this package from the public Riko-Memory repository into a DSH profile that loads the Agent preset registry and Minimal preset:

```text
github:AkinoHaruka/Riko-Memory#path:/adapters/dsh
```

In Creator mode, use `plugin_manager` with `action: install_bundle` and the specifier above as `target`. The DSH plugin page or `dsh plugin --profile <profile> add <specifier>` can also install the bundle. After installation, select **Riko** in the Agent preset selector. Minimal and other presets are untouched.

## Configure the local connection

The adapter remains disabled until all three environment variables below are present. Set them in the DSH process environment (or the DSH home `.env` file):

| Variable | Value |
|---|---|
| `AGENT_MEMORY_TOKEN_FILE` | Absolute path to the token file created by `memoryd principal add` |
| `AGENT_MEMORY_SPOOL_DIR` | Absolute path for this DSH installation's durable event spool |
| `AGENT_MEMORY_HOST_ID` | Stable, non-secret identifier for this DSH installation |

The default memory service address is `http://127.0.0.1:8791`. If the service uses another loopback port, edit the `agent-memory` row's `memoryUrl` in the profile `cordis.patch.yml`. The adapter requests D6 context-bundle injection and uses the stable Agent identity `riko` for Soul lookup. The database must advertise `context_bundle_v1`; if the capability handshake is unavailable, the plugin keeps capture/tools available but disables automatic v6 injection and reports the issue.

The token file must remain outside Git and outside the plugin package. Do not put token contents in the profile patch or environment variable; only the token file path is configured.

## Riko-App connection

The bundle also inserts a host-level `riko-app-api` route plugin. It is enabled only when both DSH environment variables below are set:

| Variable | Value |
|---|---|
| `RIKO_APP_API_TOKEN_FILE` | Absolute path to a random, app-only bearer token file. Keep it outside Git and the plugin package. |
| `RIKO_APP_SESSION_REGISTRY_FILE` | Absolute path to the bridge's persistent Riko-App session registry file. |

Riko-App uses `https://riko.asia/riko-app-api/v1` by default; the URL remains editable in App settings for development or a later domain change. Put the DSH listener behind the existing HTTPS reverse proxy and route only `/riko-app-api/` to it, preserving the full URI prefix. Keep DSH bound to loopback. The bridge token is separate from provider API keys, is stored encrypted by Android Keystore in the App, and only authorizes sessions created through this bridge.

The bridge maps its versioned HTTP API to DSH `SessionController` operations. New sessions are pinned to preset `riko`; the mobile session registry prevents the App from addressing unrelated DSH sessions. Provider credentials remain on the DSH host. This route has its own bearer authentication; it does not reuse DSH's browser launch token or browser cookie.

The same bridge exposes DSH-backed model settings for the Android Settings screen:

- `GET /model-settings` returns provider profiles, DSH settings revision, and credential configured/not-configured flags. It never returns credential values.
- `POST /model-settings/discover` asks DSH `llm.discoverModels` to list models for a configured provider endpoint.
- `POST|DELETE /model-settings/providers/{id}/credential` writes or removes a provider credential in DSH's credential store. The Android app submits a key once; it is not saved in the app or logged by the bridge.
- `POST /model-settings/custom-providers` and `PUT|DELETE /model-settings/custom-providers/{id}` create, edit, or remove a custom OpenAI/Anthropic-compatible DSH provider profile and its separately stored credential.

This feature depends on the updated Bridge package being installed and active on the DSH host. Building the Android app or passing the local bridge tests does not update the production Bridge.

Generate a token on the DSH host, save it in the configured token file with owner-only permissions, and enter the same token once in Riko-App settings. The bridge exposes connection health, model catalog and selection, Riko-App session list/create/history, prompt submission, live event stream, and cancellation. History defaults to 20 messages, allows up to 50 per page, and supports older-page cursors. Model selection is session-local for the selected Agent, while DSH also asynchronously attempts to save it as the instance default for future sessions. The live stream preserves DSH bridge event names so errors can be surfaced to the App. Do not expose DSH's raw HTTP listener directly to the public network.

## Package contents

`riko-preset.patch.yml` inserts a new `preset-riko` declaration using the current upstream Minimal composition as its base, then appends the adapter row. It does not patch `preset-minimal`. The adapter module is loaded from `./dist/index.js` relative to the patch file. `dist/` is included so Git-subdirectory installation does not need to compile TypeScript or access this repository's local DSH checkout.

The package API and peer-version range are checked against DSH `0.2.0-rc.1`, source commit `4878cdabd87d4041bdaff61d04c966883b9fd07a`. DSH's package manager checks `@deepseek-ai/dsh-*` peer ranges during bundle installation and composition; this package declares the matching `0.2.0-rc.1` range. The preset composition is also compared with the current upstream Minimal patch so its Riko layer stays compatible without changing Minimal.
