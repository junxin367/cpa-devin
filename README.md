# CPA Devin 动态模型目录

CLIProxyAPI 原生插件。它使用每个 Devin 凭证调用上游 `GetCliModelConfigs`，将上游返回的 effort 变体聚合为 CLIProxyAPI 模型与 thinking levels，并在请求时映射回真实上游 `chat_model_uid`，避免账号实际可用模型与 CLIProxyAPI 内置 Devin 目录不一致。

实现参考 [CLIProxyAPI PR #6529](https://github.com/router-for-me/CLIProxyAPI/pull/6529)，但通过插件扩展点完成，不替换 CLIProxyAPI 原生 Devin 请求执行器。

[下载发布产物](https://github.com/junxin367/cpa-devin/releases/latest) · [插件注册表](https://raw.githubusercontent.com/junxin367/cpa-devin/main/registry.json) · [查看自动构建](https://github.com/junxin367/cpa-devin/actions)

## 行为

- `model.for_auth` 按 Devin 凭证读取账号专属模型目录。
- 上游返回的精确 effort UID 会在宿主内部注册，保证鉴权选择和请求路由能命中对应凭证；插件会过滤 `/v1/models` 响应中的 effort 变体，客户端模型目录只展示聚合后的基础模型，例如 `devin/claude-sonnet-5-5`。
- `model.route` 将插件发布的模型名解析为该账号目录中的真实 `chat_model_uid`，再交给 CLIProxyAPI 原生 `devin` executor 执行。
- 使用 `api_key`、`session_token` 或 `token`，兼容 `base_url`、`device_seed`、`proxy_url` 和 `header:*` 自定义请求头。
- 账号设置了 `proxy_url` 时使用该代理直接请求；未设置时通过 CLIProxyAPI 的 `host.http.do` 执行。
- 同一基础模型的 `none`、`low`、`medium`、`high`、`xhigh`、`max` 等变体会合并为 `Thinking.Levels`。
- 上游请求失败、凭证缺失、响应无效或返回空目录时，记录宿主警告并回退到插件内置的 Devin 目录，不会让该凭证变成无模型。
- 插件只声明 OAuth 模型作用域，CLIProxyAPI 继续使用原生 `devin` executor。

## 兼容性

已按 `CLIProxyAPI v8.0.29` 的插件 ABI 实现和验证。该版本包含 `model_provider`、`model.for_auth`、`model_router`、`model.route`、`response_interceptor` 与 `executor_model_scope: oauth`。

插件会在宿主加载插件、加载或更新 Devin 凭证等触发按凭证模型注册的时机重新读取上游目录。CLIProxyAPI 当前没有向独立模型插件暴露“主动要求宿主重新发布某个凭证模型”的回调；上游目录在凭证不变期间发生变化时，需要重载插件、更新凭证或重启宿主来触发重新注册。

CPA 管理页“新增提供商模型别名”的模型下拉读取 CLIProxyAPI 内置静态目录，不读取插件按凭证发布的动态模型；宿主也不会让响应拦截器处理该管理接口，因此插件无法把动态模型注入这个下拉。动态模型不需要配置 OAuth 别名，直接使用 `/v1/models` 返回的基础模型名发起请求即可，插件会按照请求中的推理等级映射到真实上游 UID。

管理页“凭证支持的模型”接口直接展示宿主内部注册项，仍可能看到用于鉴权选择的精确 effort UID；客户端 `/v1/models` 目录不会展示这些内部变体。

## 安装

可以将本仓库的 [registry.json](https://raw.githubusercontent.com/junxin367/cpa-devin/main/registry.json) 添加到 CPA 的插件商店注册表，也可以从 [GitHub Releases](https://github.com/junxin367/cpa-devin/releases/latest) 手动下载对应平台的 ZIP。ZIP 内动态库已使用正确的插件文件名；Release 的 `checksums.txt` 用于校验平台 ZIP，ZIP 内的 `SHA256SUMS.txt` 用于校验安装用动态库。

1. 获取对应平台动态库。
2. 将动态库放入 CLIProxyAPI 的 `plugins` 目录，安装文件名必须是：

   | 平台 | 文件名 |
   | --- | --- |
   | Windows | `cpa-devin.dll` |
   | Linux | `cpa-devin.so` |
   | macOS | `cpa-devin.dylib` |

3. 在 CLIProxyAPI 配置中启用全局插件加载和本插件：

   ```yaml
   plugins:
     enabled: true
     dir: "plugins"
     configs:
       cpa-devin:
         enabled: true
   ```

4. 重启 CLIProxyAPI。
5. 检查日志中插件加载成功，且 Devin 凭证没有出现 `cpa-devin 上游模型目录读取失败` 警告。
6. 请求 `/v1/models`，确认对应 Devin 凭证发布的模型与该账号上游目录一致。

插件不需要额外 YAML 配置，也不会保存或修改 Devin 凭证。

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
models/devin_models.json   上游失败时使用的内置目录
scripts/build.ps1          构建、打包与校验
scripts/release.mjs        版本校验、发布说明、校验清单与 latest.json
registry.json              CPA 插件注册表
.github/workflows/build-release.yml  四平台自动构建与发布
```
