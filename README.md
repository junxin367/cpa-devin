# CPA Devin 动态模型目录

CLIProxyAPI 原生插件。它使用每个 Devin 凭证调用上游 `GetCliModelConfigs`，将上游返回的 effort 变体聚合为 CLIProxyAPI 模型与 thinking levels，并由原生 Devin 执行器按请求推理等级解析真实上游 `chat_model_uid`，避免账号实际可用模型与 CLIProxyAPI 内置 Devin 目录不一致。

实现参考 [CLIProxyAPI PR #6529](https://github.com/router-for-me/CLIProxyAPI/pull/6529)，但通过插件扩展点完成，不替换 CLIProxyAPI 原生 Devin 请求执行器。

[下载发布产物](https://github.com/junxin367/cpa-devin/releases/latest) · [插件注册表](https://raw.githubusercontent.com/junxin367/cpa-devin/main/registry.json) · [查看自动构建](https://github.com/junxin367/cpa-devin/actions)

## 行为

- `model.for_auth` 按 Devin 凭证读取账号专属模型目录。
- 凭证只注册聚合后的基础模型，例如 `devin/claude-sonnet-5-5`；`-low`、`-high`、`-thinking`、`-thinking-1m`、`-fast` 等上游内部 UID 不再进入凭证模型列表。
- 基础模型请求（例如 `devin/claude-sonnet-5-5`）不经过插件改名，也不会自动追加 `(medium)` 等后缀。请求体中的推理参数保持不变，由 CLIProxyAPI 原生 `devin` executor 结合 Devin 目录选择真实 `chat_model_uid`，使额度准入与用量通知使用相同的基础模型名。
- `model.route` 仅兼容调用方显式填写的变体或等级后缀；普通基础模型请求交还原生流程，不需要注册精确 effort UID。
- `response_interceptor` 会从 `/v1/models` 中过滤仍由其他来源发布的内部 UID，避免客户端模型目录重复展示变体。
- 使用 `api_key`、`session_token` 或 `token`，兼容 `base_url`、`device_seed`、`proxy_url` 和 `header:*` 自定义请求头。
- 账号设置了 `proxy_url` 时使用该代理直接请求；未设置时通过 CLIProxyAPI 的 `host.http.do` 执行。
- 同一基础模型的 `none`、`low`、`medium`、`high`、`xhigh`、`max` 等变体会合并为 `Thinking.Levels`。
- 上游请求失败、凭证缺失、响应无效或返回空目录时，记录宿主警告并回退到插件内置的 Devin 目录，不会让该凭证变成无模型。
- 插件只声明 OAuth 模型作用域，CLIProxyAPI 继续使用原生 `devin` executor。

## 兼容性

已按 `CLIProxyAPI v8.0.29` 的插件 ABI 实现和验证。该版本包含 `model_provider`、`model.for_auth`、`model_router`、`model.route`、`response_interceptor` 与 `executor_model_scope: oauth`。

插件会在宿主加载插件、加载或更新 Devin 凭证等触发按凭证模型注册的时机重新读取上游目录。CLIProxyAPI 当前没有向独立模型插件暴露“主动要求宿主重新发布某个凭证模型”的回调；上游目录在凭证不变期间发生变化时，需要重载插件、更新凭证或重启宿主来触发重新注册。

基础模型路由依赖 CLIProxyAPI 的 Devin 模型目录。仓库中的 [`models/devin_models.json`](models/devin_models.json) 在宿主官方目录基础上补充插件已支持的新模型；必须按下方安装步骤配置 `models.devin-catalog`，否则宿主无法为官方目录尚未收录的新模型解析推理等级。

本插件不修改或注入 OAuth 模型别名。

## 安装

可以将本仓库的 [registry.json](https://raw.githubusercontent.com/junxin367/cpa-devin/main/registry.json) 添加到 CPA 的插件商店注册表，也可以从 [GitHub Releases](https://github.com/junxin367/cpa-devin/releases/latest) 手动下载对应平台的 ZIP。ZIP 内动态库已使用正确的插件文件名；Release 的 `checksums.txt` 用于校验平台 ZIP，ZIP 内的 `SHA256SUMS.txt` 用于校验安装用动态库。

1. 获取对应平台动态库。
2. 将动态库放入 CLIProxyAPI 的 `plugins` 目录，安装文件名必须是：

   | 平台 | 文件名 |
   | --- | --- |
   | Windows | `cpa-devin.dll` |
   | Linux | `cpa-devin.so` |
   | macOS | `cpa-devin.dylib` |

3. 在 CLIProxyAPI 配置顶层指定本插件维护的 Devin 模型目录，并启用全局插件加载和本插件：

   ```yaml
   models:
     devin-catalog: "https://raw.githubusercontent.com/junxin367/cpa-devin/main/models/devin_models.json"

   plugins:
     enabled: true
     dir: "plugins"
     configs:
       cpa-devin:
         enabled: true
   ```

4. 重启 CLIProxyAPI。
5. 检查日志中插件加载成功，且 Devin 凭证没有出现 `cpa-devin 上游模型目录读取失败` 警告。
6. 打开凭证模型列表或请求 `/v1/models`，确认只展示基础模型；使用请求体中的 `reasoning_effort`、`reasoning.effort`、`reasoning.level` 或 `thinking.level` 选择推理等级。

`models.devin-catalog` 支持配置热加载；保存配置后仍建议重载插件或重启宿主，使已有 Devin 凭证立即重新注册为基础模型。插件不会保存或修改 Devin 凭证。

## 从源码构建

需要 Rust stable 和 PowerShell 7：

```powershell
pwsh -File scripts/build.ps1
```

脚本执行 `cargo test --locked` 和 release 构建，并生成当前平台安装包与 SHA-256 校验文件。

推送与 `Cargo.toml` 版本一致的 `v*` 标签后，GitHub Actions 会执行源码检查并构建 Windows amd64、Linux amd64、macOS amd64 和 macOS arm64 四个平台，全部成功后创建 GitHub Release、`checksums.txt` 与 CPA 插件商店使用的 `latest.json`。

也可以直接构建：

```powershell
cargo build --release --locked
```

Windows 原始产物位于 `target/release/cpa_devin.dll`，安装时需要重命名为 `cpa-devin.dll`。

## 源码结构

```text
src/lib.rs                 C ABI、插件注册与 RPC 分发
src/host.rs                CLIProxyAPI 宿主回调桥接
src/devin.rs               凭证解析、上游请求、protobuf 与模型聚合
models/devin_models.json   宿主 Devin 目录源与插件内置回退目录
scripts/build.ps1          构建、打包与校验
scripts/release.mjs        版本校验、发布说明、校验清单与 latest.json
registry.json              CPA 插件注册表
.github/workflows/build-release.yml  四平台自动构建与发布
```
