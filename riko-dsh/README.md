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

The single authoritative source of truth for agent personality is the kernel Soul (`GET /v1/soul`, injected as `riko-dsh:soul` at order 10300, isolated per agent).

The bundle includes an editable `soul.md` at the `@riko/dsh` package root for optional local override. File-based personality injection (`twin-soul` at order 100) is deprecated and disabled by default. If file-based override is needed, explicitly opt in via `fileSoulEnabled: true` (or `RIKO_DSH_FILE_SOUL=1`). When enabled, it routes through the Soul assembly path (`riko-dsh:soul` at order 10300) with identical per-agent and child-session isolation semantics, rather than injecting as a parallel section.

## Relationship guidance

Kernel alignment synthesis from `memoryd` (`POST /v1/context/bundle` segment `alignment`) is the single authoritative source of truth. The static `alignment.md` file is deprecated and preserved only as a static reference.

## Riko-App API Bridge

The Android app's HTTP bridge is a separate DSH bundle in the repository's `riko-app-bridge/` directory. Install it independently from this memory adapter. It owns the `/riko-app-api/v1` routes, model settings proxy, app-only bearer token, and Riko-App session registry. See `../../../riko-app-bridge/README.md` for installation and configuration. Installing this memory bundle alone does not install or activate the Bridge.

## Package contents

`riko-preset.patch.yml` inserts a new `preset-riko` declaration using the current upstream Minimal composition as its base, then appends the memory adapter row. It does not patch `preset-minimal`. The adapter module is loaded from `./dist/index.js` relative to the patch file. `dist/` is included so Git-subdirectory installation does not need to compile TypeScript or access this repository's local DSH checkout.

The package API and peer-version range are checked against DSH `0.2.1-rc.2`, source commit `687f282e8625c6b8d11ff58211a90524be2e4317`, and Cordis `4.0.5`. The development dependencies link to that local DSH checkout. DSH's package manager checks `@deepseek-ai/dsh-*` peer ranges during bundle installation and composition. The Riko preset remains separate from Minimal.
