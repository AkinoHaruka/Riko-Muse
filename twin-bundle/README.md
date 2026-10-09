---
description: "A dsh profile bundle with four independently switchable Twin plugin rows for memory, persona, reflection, and companion behavior."
kind: "package-bundle"
---

# @deepseek-ai/dsh-twin-bundle

English | [中文](README.zh.md)

## Summary

This bundle adds four independently switchable Twin plugin rows to a dsh profile. Install it from this checkout to mount stubs for memory, persona, background reflection, and companion behavior. The bundle leaves the default agent loop in place. Each row can be disabled in the profile's `cordis.patch.yml`; `twin-memory` is reserved for a later connection to Riko-Memory's `memoryd` HTTP API.

## Table of Contents

- [Use this package](#use-this-package)
- [Understand the implementation](#understand-the-implementation)
- [Further Exploration](#further-exploration)
- [Model Experience](#model-experience)
- [Known Limitations and Deferred Work](#known-limitations-and-deferred-work)
- [Dev Note](#dev-note)

-----

<a id="use-this-package"></a>
## Use this package

### Install into a profile

From the DSH repository root, add this checkout to a profile and restart that profile:

```sh
dsh plugin --profile <name> add ./packages/bundle/twin-bundle
dsh plugin --profile <name> remove @deepseek-ai/dsh-twin-bundle
```

The bundle contributes four plugin rows. A profile patch can disable one row by id; a later patch still leaves its sibling rows enabled:

```yaml
- id: twin-memory
  disabled: true
```

The profile patch targets one inserted row at a time. If a row gains configuration fields, restate the fields that should remain when replacing its configuration.

### What you get

The bundle mounts four inert plugin stubs for memory, persona, background reflection, and companion behavior. Each row has an independent plugin entry point and can be enabled or disabled without changing the other rows.

-----

<a id="understand-the-implementation"></a>
## Understand the implementation

<details>
<summary>Implementation internals — click to expand</summary>

[`cordis.patch.yml`](cordis.patch.yml) inserts four rows whose `name` fields resolve to this package's exported subpaths. Each plugin entry is loadable but currently contributes no runtime behavior. See the module READMEs for their responsibilities and planned integration points.

| File | Role |
|---|---|
| [`src/twin-memory/index.ts`](src/twin-memory/index.ts) | Memory plugin entry and [module README](src/twin-memory/README.md) |
| [`src/twin-soul/index.ts`](src/twin-soul/index.ts) | Persona plugin entry and [module README](src/twin-soul/README.md) |
| [`src/twin-dream/index.ts`](src/twin-dream/index.ts) | Background reflection plugin entry and [module README](src/twin-dream/README.md) |
| [`src/twin-companion/index.ts`](src/twin-companion/index.ts) | Companion behavior entry and [module README](src/twin-companion/README.md) |

</details>

-----

<a id="further-exploration"></a>
## Further Exploration

- [Profile bundles](../README.md) — package map for `dsh` profile layers.
- [App boot](../../boot/app-boot/README.md#profiles) — profile composition and patch order.

-----

<a id="model-experience"></a>
## Model Experience

None, as the stubs add no model-facing behavior.

#### KV Cache effect

None; the bundle contributes no prompt or tool data.

## Known Limitations and Deferred Work

- **Inert stubs** — the four plugin entries collect no Session events, contribute no prompt sections, register no tools, schedule no work, and do not steer agent turns. The module READMEs identify the next integration point for each plugin.

<a id="dev-note"></a>
### Dev Note

<details>
<summary>Working context for maintainers — click to expand</summary>

None.

</details>
