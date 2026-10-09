---
description: "包含四个可独立开关的 Twin 插件行的 dsh profile 组合包，覆盖记忆、人格、复盘与陪伴行为。"
kind: "package-bundle"
---

# @deepseek-ai/dsh-twin-bundle

[English](README.md) | 中文

## 概述

此组合包向 dsh profile 添加四个可独立开关的 Twin 插件行。从此仓库安装后，会挂载记忆、人格、后台复盘与陪伴行为的 stub。组合包保留默认 agent loop。每行都能在 profile 的 `cordis.patch.yml` 中禁用；`twin-memory` 预留给后续对接 Riko-Memory 的 `memoryd` HTTP API。

## 目录

- [使用此包](#use-this-package)
- [了解实现](#understand-the-implementation)
- [进一步探索](#further-exploration)
- [模型体验](#model-experience)
- [已知限制与后续工作](#known-limitations-and-deferred-work)
- [开发备注](#dev-note)

-----

<a id="use-this-package"></a>
## 使用此包

### 安装到 profile

在 DSH 仓库根目录中，将此 checkout 添加到一个 profile 并重启该 profile：

```sh
dsh plugin --profile <name> add ./packages/bundle/twin-bundle
dsh plugin --profile <name> remove @deepseek-ai/dsh-twin-bundle
```

组合包提供四个插件行。profile patch 可按 id 禁用其中一行；后续 patch 不会禁用其他行：

```yaml
- id: twin-memory
  disabled: true
```

profile patch 每次只定位一个已插入行。如果某行增加配置字段，替换其配置时要重述需要保留的字段。

### 可用内容

组合包挂载四个空插件 stub，分别用于记忆、人格、后台复盘与陪伴行为。每行都有独立插件入口，可以单独开关而不改变其他行。

-----

<a id="understand-the-implementation"></a>
## 了解实现

<details>
<summary>实现细节——点击展开</summary>

[`cordis.patch.yml`](cordis.patch.yml) 插入四行，其 `name` 字段解析到本包导出的子路径。每个插件入口都可加载，但当前不贡献运行时行为。模块 README 说明各自职责与后续对接点。

| 文件 | 职责 |
|---|---|
| [`src/twin-memory/index.ts`](src/twin-memory/index.ts) | 记忆插件入口与[模块 README](src/twin-memory/README.zh.md) |
| [`src/twin-soul/index.ts`](src/twin-soul/index.ts) | 人格插件入口与[模块 README](src/twin-soul/README.zh.md) |
| [`src/twin-dream/index.ts`](src/twin-dream/index.ts) | 后台复盘插件入口与[模块 README](src/twin-dream/README.zh.md) |
| [`src/twin-companion/index.ts`](src/twin-companion/index.ts) | 陪伴行为入口与[模块 README](src/twin-companion/README.zh.md) |

</details>

-----

<a id="further-exploration"></a>
## 进一步探索

- [Profile 组合包](../README.zh.md)——`dsh` profile 层的包索引。
- [App boot](../../boot/app-boot/README.zh.md#profiles)——profile 组合与 patch 顺序。

-----

<a id="model-experience"></a>
## 模型体验

无；这些 stub 不添加面向模型的行为。

#### KV Cache 影响

无；组合包不会贡献提示词或工具数据。

## 已知限制与后续工作

- **空 stub**——这四个插件入口不会采集 Session 事件、贡献提示词片段、注册工具、安排任务或引导 agent 回合。各模块 README 说明对应插件的下一步对接点。

<a id="dev-note"></a>
### 开发备注

<details>
<summary>维护者工作背景——点击展开</summary>

无。

</details>
