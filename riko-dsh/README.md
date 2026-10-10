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

## Twin personality

The bundle includes an editable `soul.md` at the `@riko/dsh` package root. It is injected as the `twin-soul` system prompt section and watched while DSH is running, so saving the file takes effect without a restart. Set `RIKO_DSH_SOUL_FILE` to an absolute or relative path to use a different local file. The file-based section is independent of the API-backed `riko-dsh:soul` section; both are included in the assembled prompt.

## Riko-App API Bridge

The Android app's HTTP bridge is a separate DSH bundle in the repository's `riko-app-bridge/` directory. Install it independently from this memory adapter. It owns the `/riko-app-api/v1` routes, model settings proxy, app-only bearer token, and Riko-App session registry. See `../../../riko-app-bridge/README.md` for installation and configuration. Installing this memory bundle alone does not install or activate the Bridge.

## Package contents

`riko-preset.patch.yml` inserts a new `preset-riko` declaration using the current upstream Minimal composition as its base, then appends the memory adapter row. It does not patch `preset-minimal`. The adapter module is loaded from `./dist/index.js` relative to the patch file. `dist/` is included so Git-subdirectory installation does not need to compile TypeScript or access this repository's local DSH checkout.

The package API and peer-version range are checked against DSH `0.2.0-rc.1`, source commit `4878cdabd87d4041bdaff61d04c966883b9fd07a`. DSH's package manager checks `@deepseek-ai/dsh-*` peer ranges during bundle installation and composition; this package declares the matching `0.2.0-rc.1` range. The preset composition is also compared with the current upstream Minimal patch so its Riko layer stays compatible without changing Minimal.
