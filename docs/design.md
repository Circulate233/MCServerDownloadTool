# MCServerDownloadTool 设计

## 范围

MCServerDownloadTool 将严格的 `server-install.json` 转换成可启动的 Minecraft 服务端目录。当前可执行程序已经连接完整安装链：配置解析、manifest 校验、Java 发现与选择、共享下载、手动文件门禁、官方 Loader 安装、启动产物验证、脚本生成和持久状态写入。

不在范围内：自动下载 Java、服务端进程守护、世界备份、在线升级、自动接受 EULA，以及在启动脚本中执行下载或校验。

## 执行链

```mermaid
flowchart LR
    CLI["CLI / locale / proxy"] --> Manifest["读取并严格校验 server-install.json"]
    Manifest --> Lock["获取 .mcsdt 安装锁"]
    Lock --> Java["优先验证状态 Java，否则发现并选择"]
    Java --> Inspect["校验已有文件、状态和 Loader 输出"]
    Inspect --> Download["共享引擎并发下载自动文件与 Loader installer"]
    Download --> Manual["统一检查手动文件"]
    Manual --> Loader["运行或复用官方 Loader 安装结果"]
    Loader --> Output["严格验证最终启动产物"]
    Output --> Commit["原子写入启动脚本和 install-state.json"]
```

默认 manifest 是当前可执行文件同目录的 `server-install.json`。显式 `--manifest` 可以选择其他文件；安装根目录始终取所选 manifest 的父目录，不受当前工作目录影响。

语言优先级为 CLI、系统 locale、英文。代理优先级为 CLI、`HTTPS_PROXY`、`ALL_PROXY`、`HTTP_PROXY`，之后是对应小写变量。显式配置的第一个无效代理直接失败，不跳过并尝试更低优先级来源。

## Manifest v1

顶层字段为 `schema_version`、`minecraft`、`java`、`loader`、可选 `curseforge_api_key` 和 `files`。所有对象使用 `deny_unknown_fields`，语义校验 fail-fast。

### Minecraft 与 Java

- `minecraft.version` 必须非空。
- `java.major` 为 `1..=255`。
- `min_memory_mb` 大于零且不超过 `max_memory_mb`。
- `jvm_args` 和 `server_args` 是独立参数数组；空白项、NUL 和换行被拒绝。

Manifest 不保存本机 Java 路径。安装状态中的 Java 先经过绝对路径、普通文件、无链接/reparse 和版本 probe 验证，失效才进入完整发现。Java 层规范化去重后并行执行 `java -XshowSettings:properties -version`，只保留主版本完全匹配的运行时。Java 8 的 `1.8` 映射为主版本 8。工具不提供 managed JDK 下载兜底。

发现来源覆盖绝对 PATH 项、`JAVA_HOME`/`JRE_HOME`、平台 JDK 目录、厂商注册表、SDKMAN、asdf、Gradle/IDE toolchains 和 Minecraft runtime，但不扫描安装根 `runtime`。候选、扫描 entry、warning、worker 和 probe 输出均有硬上限。

### Loader

支持 `forge`、`fabric`、`neoforge`、`cleanroom`，均要求 Maven 安全的精确版本。Installer URL 只能使用对应官方 origin 和精确坐标；Cleanroom 另允许官方 GitHub Release 精确路径。禁止端口、userinfo、query 和 fragment；inline SHA-1 与精确追加 `.sha1` 的 sidecar 二选一，未知 size 也受 512 MiB 上限。

执行契约：

- Forge、NeoForge、Cleanroom：`java -jar installer --installServer`。
- Fabric：`java -jar installer server -mcversion <MC> -loader <VERSION> -downloadMinecraft`。
- HTTP/HTTPS proxy 作为拆分后的 JVM system properties 传入；SOCKS proxy 在需要执行上游 installer 时明确拒绝。
- stdout/stderr 按行实时转发；单行 64 KiB、每流 16 MiB、总运行 30 分钟。清除 JVM 注入环境，失败时回收 Unix process group/Windows Job Object 中的完整进程树。

Loader 返回成功后仍必须验证 manifest 声明的精确输出。Forge/NeoForge 可以使用 `modern_args` 的 Windows/Unix 参数文件；`exact_jar` 检查非空精确路径，Fabric 还检查 JAR manifest 的 `Main-Class`。禁止模糊扫描 JAR 名称。

### Files

文件记录由 `name`、`type`、`path`、`download`、`project_page`、`sha1`、`size` 组成。`type` 仅接受 `mod`、`resource_pack`、`shader_pack`；`download` 是带 `mode` discriminator 的 `automatic { url }` 或 `manual`。

路径只接受 `/` 分隔的规范相对路径，并拒绝：

1. 绝对路径、盘符、反斜杠、NUL、空组件、`.` 和 `..`；
2. 末尾空格/点和 Windows 设备名；
3. ASCII 大小写折叠后的重复目标；
4. `server-install.json`、工具二进制、启动脚本、`missing-files.txt` 和 `.mcsdt` 命名空间。

自动 URL 必须匹配精确 ForgeCDN origin/path，`project_page` 必须匹配文件类型对应的 CurseForge category/slug/file ID。Manifest 最大 8 MiB、最多 20,000 个文件；参数数组各最多 512 项，单项最多 8 KiB。

自动文件与 Loader installer 进入同一批下载。随后一次性检查全部 manual 文件；失败项写入原子更新的 `missing-files.txt` 后终止，不会运行 Loader。全部补齐时删除过期清单。

### CurseForge key

只有精确 ForgeCDN 自动 URL 才允许并要求顶层 `curseforge_api_key`。请求构建器将 `x-api-key` 标记为敏感 header，并绑定到精确 origin；逐跳重定向重新计算 header，跨源不发送。Secret 的 `Debug` 输出被替换为 `[REDACTED]`，URL 和错误日志也执行脱敏。

## 下载架构

整个安装只创建一个 `NetworkEngine`，Loader SHA-1 sidecar、Loader installer 和服务端自动文件共享 HTTP client、连接池、重试与请求预算。Loader installer 自身下载依赖的行为仍归上游 Java installer。

自适应并发公式：

```text
global_requests   = clamp(cpu * 4, 8, 64)
requests_per_host = min(global, clamp(cpu * 2, 4, 32))
requests_per_file = min(per_host, clamp(cpu, 2, 16))
```

`available_parallelism()` 失败时终止，不能使用隐藏固定值。硬上限、有限 worker queue 和有限 observer channel 防止资源或日志无限增长。

网络层使用 blocking reqwest、HTTP/2 adaptive window、连接池、显式 redirect、指数退避及 `Retry-After`。达到阈值的大文件先探测 Range。响应完成 size/hash 契约后在最终目标同目录原子发布，安装层再独立复验，不经过第二份 staging 和完整 copy；失败不覆盖正确目标。

## 幂等状态与文件系统

`.mcsdt/install-state.json` 原子记录：

- 原始 manifest 字节的 SHA-256；
- 选定 Java executable；
- 完整 Loader 计划摘要；
- 验证后的 Loader 启动描述；
- 每个 Loader 最终产物的相对路径、size 和 SHA-256；
- 安装器生成脚本的 SHA-256。

每次运行获取独占 `.mcsdt/install.lock`。已存在文件只有 size 与 SHA-1 都匹配才复用。Loader 仅在计划摘要、Java identity、启动描述和所有产物 size/SHA-256 同时匹配时复用。

启动脚本发布采用所有权检测：不存在则创建；内容与本次生成一致则复用；内容仍匹配上次生成摘要时可更新；用户修改过时保留原文件，将候选写到 `.new` 并返回明确冲突。状态、手动清单和文件发布使用原子写入或同文件系统临时路径。

## 启动脚本

Windows 生成 `start.bat`，Unix 生成可执行 `start.sh`。两者只包含：

- 用户选定 Java 的绝对路径；
- `-Xms`/`-Xmx`；
- manifest JVM 参数；
- 验证后的 args file 或精确 `-jar` 目标；
- manifest 服务端参数。

脚本不下载、不校验、不调用安装器、不写 EULA。Windows 使用 System32 Windows PowerShell 绝对路径检测 Explorer 控制台，避免当前目录/PATH 劫持；Unix 使用单引号安全转义和 `exec`。

## 错误与日志

错误映射到稳定退出类别且不得吞掉。持久日志使用有界缓冲并合并高频进度；阶段、失败、完成和会话结束执行 durability flush/sync，日志路径拒绝 symlink、reparse 和多硬链接。

## CI 与发布

`.github/workflows/ci.yml` 在非 `v*` push 和 pull request 上执行质量检查并构建三平台 binary；`v*` 只触发 release workflow，避免重复构建。

`.github/workflows/release.yml` 只由 `v*` tag 触发。tag 去掉小写 `v` 后必须是严格 SemVer 2.0.0，并作为发布版本权威来源；Cargo 保持 `0.0.0` 占位版本。发布流程构建并 smoke test 三个平台资产、生成摘要/index/provenance，并创建非 draft Release。

所有外部 `uses:` 引用固定 40 位 commit SHA，并由 `Test-ActionPins.ps1` 静态校验。

## MCModPackUtil 发布边界

`MCModPackUtil` 将 GitHub Releases 的最新正式 `v*` tag 视为安装器唯一来源。它不会读取本仓库工作树、分支提交或短期 CI artifact。安全修复只有在新的不可变 release tag 创建完成后才会被自动解析和嵌入服务端包；已发布 tag 与资产不得移动或覆盖，必须递增版本发布修复。
