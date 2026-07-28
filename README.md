# MCServerDownloadTool

MCServerDownloadTool 是一个纯终端、数据驱动的 Minecraft 服务端安装器。它读取严格的 `server-install.json`，选择符合要求的本机 Java，并发下载并校验 Loader 安装器与服务端文件，运行官方 Loader 安装器，最后生成不包含下载逻辑的启动脚本。

工具不会自动下载 Java、接受 Minecraft EULA、启动服务端或修改 manifest。安装根目录始终是所选 manifest 所在目录。

## 支持平台

| 平台 | Rust 目标 | Release 资产 |
| --- | --- | --- |
| Windows x64 | `x86_64-pc-windows-msvc` | `MCServerDownloadTool-windows-x86_64.exe` |
| Linux x64 | `x86_64-unknown-linux-musl` | `MCServerDownloadTool-linux-x86_64` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `MCServerDownloadTool-macos-aarch64` |

Linux 使用 musl 目标。macOS 只提供 Apple Silicon 资产，且当前未使用 Apple Developer ID 签名或公证；GitHub provenance 可验证构建来源，但不能替代 Apple 平台签名。

## 快速开始

1. 从 GitHub Release 下载当前平台的安装器及同名 `.sha256`，校验 SHA-256。Linux/macOS 下载后执行 `chmod 755 <安装器>`。
2. 将安装器和 `server-install.json` 放进目标服务端目录。
3. 直接运行安装器，或在终端中执行：

```powershell
.\MCServerDownloadTool-windows-x86_64.exe --lang zh-CN
```

```bash
./MCServerDownloadTool-linux-x86_64 --lang zh-CN
```

4. 安装器会搜索本机 Java，并列出与 `java.major` 完全一致的候选。输入序号选择；没有候选时，输入 Java 可执行文件或 Java Home。
5. 若生成 `missing-files.txt`，按其中的路径、项目页、大小和 SHA-1 补齐手动文件，再次运行安装器。
6. 安装成功后使用同目录的 `start.bat` 或 `start.sh` 启动服务端。Minecraft EULA 由服务端首次启动流程处理。

重复运行是幂等修复：大小和 SHA-1 正确的文件会复用，损坏或缺失的自动文件会重新下载；安装状态和 Loader 输出仍有效时跳过 Loader 安装。若用户修改过启动脚本，安装器保留原文件并写出 `start.bat.new` 或 `start.sh.new`，随后明确报告冲突。

## CLI

```text
mc-server-download-tool [OPTIONS]

Options:
  --manifest <PATH>       JSON manifest 路径
  --lang <en-US|zh-CN>    输出语言
  --proxy <URL>           HTTP、HTTPS、SOCKS5 或 SOCKS5H 代理
  -h, --help              显示帮助
  -V, --version           显示版本
```

省略 `--manifest` 时，默认读取**可执行文件同目录**的 `server-install.json`，不是当前工作目录。显式指定其他清单时，清单所在目录就是安装根目录：

```bash
mc-server-download-tool --manifest ./staging/server-install.json --lang zh-CN
```

语言优先级为 `--lang`、系统 locale、`en-US`。中文 locale 使用 `zh-CN`，其他 locale 使用英文。

代理优先级为：

1. `--proxy`
2. `HTTPS_PROXY`
3. `ALL_PROXY`
4. `HTTP_PROXY`
5. 对应的小写环境变量

代理 URL 必须包含 host，只允许 `http`、`https`、`socks5`、`socks5h`，且不能包含凭据、query 或 fragment。Rust 下载引擎支持以上代理；官方 Loader 安装器仅能安全传递 HTTP/HTTPS JVM 代理参数，因此需要执行 Loader 安装器时使用 SOCKS 代理会明确失败。

## Manifest v1

`server-install.json` 必须是 UTF-8 JSON，`schema_version` 只能为 `1`，所有对象都拒绝未知字段。仓库根目录的 [server-install.json](server-install.json) 是可解析的 Cleanroom 示例。

结构示例：

```json
{
  "schema_version": 1,
  "minecraft": {
    "version": "1.21.1"
  },
  "java": {
    "major": 21,
    "min_memory_mb": 2048,
    "max_memory_mb": 4096,
    "jvm_args": ["-XX:+UseG1GC"],
    "server_args": ["nogui"]
  },
  "loader": {
    "kind": "fabric",
    "version": "0.16.10",
    "installer": {
      "url": "https://example.invalid/fabric-installer.jar",
      "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "size": 1024
    },
    "output": {
      "type": "exact_jar",
      "path": "fabric-server-launch.jar",
      "main_class": "net.fabricmc.loader.impl.launch.server.FabricServerLauncher"
    }
  },
  "files": [
    {
      "name": "Example Mod",
      "type": "mod",
      "path": "mods/example.jar",
      "download": {
        "mode": "automatic",
        "url": "https://example.invalid/example.jar"
      },
      "project_page": "https://example.invalid/project/files/1",
      "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "size": 2048
    }
  ]
}
```

示例域名和摘要只用于说明格式。

### 顶层字段

| 字段 | 必需 | 作用 |
| --- | --- | --- |
| `schema_version` | 是 | 当前只能为整数 `1` |
| `minecraft.version` | 是 | 精确 Minecraft 版本 |
| `java` | 是 | Java 主版本、堆内存、JVM 参数和服务端参数 |
| `loader` | 是 | Loader 类型、精确版本、安装器和预期启动产物 |
| `files` | 是 | 安装到服务端根目录下的文件数组，可为空 |
| `curseforge_api_key` | 条件 | 仅当自动下载 URL 位于 `forgecdn.net` 或其子域时必需；其他情况下禁止出现 |

### Java

- `major` 必须为 `1..=255`，候选 Java 必须报告完全相同的主版本；Java 8 的 `1.8` 会识别为主版本 8。
- `min_memory_mb` 必须大于零且不大于 `max_memory_mb`。
- `jvm_args` 位于启动目标之前，`server_args` 位于启动目标之后；两者都是参数数组，不按 shell 字符串解析。
- 安装器不会下载 Java。候选来自 PATH、Java 环境变量、常见 JDK/JRE 安装目录、厂商注册表项、SDKMAN、asdf、Gradle/IDE toolchain 和 Minecraft runtime 等平台来源。
- Java 候选会规范化去重，并通过 `java -XshowSettings:properties -version` 并行验证版本、厂商和架构；用户始终按序号选择，或在无候选时手动指定并验证路径。

### Loader

`loader.kind` 支持 `forge`、`fabric`、`neoforge`、`cleanroom`。`version` 必须是非空的精确版本。

`loader.installer` 必须提供无凭据的绝对 HTTPS JAR URL，并且在 `sha1` 与 `sha1_sidecar` 中恰好提供一个。sidecar 必须与安装器同源；`size` 可省略，出现时必须大于零。

Loader 安装命令：

- Forge、NeoForge、Cleanroom：`java -jar <installer> --installServer`
- Fabric：官方 installer 的 `server -mcversion <MC> -loader <VERSION> -downloadMinecraft`

`loader.output` 声明安装完成后必须存在的精确启动产物：

- `modern_args`：仅适用于 Forge/NeoForge，分别指定 Windows 和 Unix 参数文件。
- `exact_jar`：指定精确 JAR；Fabric 还必须指定并验证 JAR manifest 的 `Main-Class`。

安装器不会按近似文件名猜测启动产物。

### 文件

每个 `files[]` 项包含：

- `name`：手动文件报告中的名称。
- `type`：`mod`、`resource_pack` 或 `shader_pack`。
- `path`：服务端根目录下使用 `/` 的规范化相对路径。
- `download`：自动 URL 或手动获取模式。
- `project_page`：供用户查找文件的绝对 HTTPS 页面。
- `sha1`：40 位十六进制 SHA-1。
- `size`：精确字节数，必须大于零。

自动下载：

```json
"download": {
  "mode": "automatic",
  "url": "https://edge.forgecdn.net/files/1234/56/example.jar"
}
```

手动下载：

```json
"download": {
  "mode": "manual"
}
```

安装器先下载所有自动资源，再统一验证手动资源。缺失或校验失败的手动文件会写入根目录 `missing-files.txt` 并终止本次安装；补齐后再次运行即可继续。

路径拒绝绝对路径、盘符、反斜杠、`.`/`..`、空组件、Windows 设备名、大小写不敏感的重复目标，以及 `.mcsdt`、启动脚本、安装器和 manifest 等保留路径。

### CurseForge API key

当自动 URL 的 host 为 `forgecdn.net` 或其子域时，顶层必须提供 `curseforge_api_key`。该值只作为敏感 `x-api-key` header 发送到声明 URL 的同源请求；跨源重定向不会携带它，调试输出和错误消息也不会显示 key。

Manifest 可能包含凭据，不应提交包含真实 key 的服务器清单。

## 安装产物

安装成功后会生成：

```text
<server-root>/
  start.bat | start.sh
  .mcsdt/
    install-state.json
    install.lock
    installers/
    staging/
```

`install-state.json` 记录 manifest SHA-256、Java 可执行文件、Loader 计划摘要、验证后的启动产物和生成脚本摘要，用于幂等复用。启动脚本只包含选定 Java 的绝对路径、内存、JVM 参数、精确 Loader 启动目标和服务端参数；它不会下载、校验、调用安装器或处理 EULA。

Windows 脚本仅在检测到独占控制台且启动失败时等待按键，终端调用仍保留正常退出码。Unix 脚本使用安全 quoting 和 `exec`，并设置可执行权限。

## 下载引擎

Loader 安装器和全部自动文件共用一个进程级下载引擎：HTTP/2、连接池、显式重定向策略、重试、Range 分段、SHA-1/大小校验、临时文件和原子发布均由同一实现处理。正确文件在发起请求前复用。

并发由 `available_parallelism()` 计算；无法获取系统并行度时明确失败：

```text
global   = clamp(cpu * 4, 8, 64)
per-host = min(global, clamp(cpu * 2, 4, 32))
per-file = min(per-host, clamp(cpu, 2, 16))
```

大文件在服务器支持可靠 Range 时按文件大小和预算分段；服务器忽略或不满足 Range 契约时会回到完整传输，而不会发布未经验证的部分文件。官方 Loader 安装器内部的库下载仍由上游 Java 安装器负责。

## 发布资产

每个正式 Release 包含三个固定名称的原始平台二进制、每个二进制对应的 `.sha256`、`release-index.json` 和 `provenance.sigstore.json`。`release-index.json` 记录版本、tag、commit、平台、目标、文件大小、SHA-256 和下载 URL。

推送严格匹配 Cargo version 的 `v*` SemVer tag 会在 Actions 中创建非草稿 GitHub Release。详细流程见 [docs/releasing.md](docs/releasing.md)。

## 从源码构建

需要 Rust stable toolchain 和 Cargo：

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --release --locked
```

普通 `push` 和 `pull_request` 会运行格式、发布脚本测试、Rust 测试和 clippy，并构建及 smoke test Windows x64、Linux x64 musl、macOS Apple Silicon 三个平台的短期 Actions artifacts。
