# 发布流程

## 前置条件

- 默认分支 CI 全部通过；
- 工作树中的 `Cargo.toml` package version 是要发布的严格 SemVer；
- `Cargo.lock` 已提交且 `cargo build --locked` 成功；
- README、`server-install.json` schema 和用户可见 CLI 变更已同步；
- GitHub Actions 允许 `contents: write`、`attestations: write` 和 OIDC `id-token: write`。

## 创建发布

以 `1.4.0` 为例：

```bash
git tag -s v1.4.0 -m "MCServerDownloadTool v1.4.0"
git push origin v1.4.0
```

工作流接受完整 SemVer 2.0.0，例如 `v1.4.0-rc.1`；拒绝 `v1.4`、`v01.4.0`、大写 `V1.4.0` 以及与 Cargo 版本不一致的 tag。是否要求本地 GPG 签名由仓库 tag 保护策略决定，workflow 自身使用 `--verify-tag` 确认远端 tag 存在。

tag push 后，Release workflow 会：

1. 严格校验 tag 与 Cargo version；
2. 重新运行 fmt、发布脚本自测、Rust test 和 clippy；
3. 构建、smoke test 并暂存三个固定名称的平台二进制及各自 `.sha256`；
4. 聚合并复算大小与 SHA-256；
5. 生成 release index 和 GitHub build provenance；
6. 创建非 draft GitHub Release 并上传全部资产。

## 验收

在 GitHub Release 页面确认：

- 资产包含三个平台二进制、三个同名 `.sha256`、`release-index.json` 和 `provenance.sigstore.json`；
- 三个平台二进制名称分别为 `MCServerDownloadTool-windows-x86_64.exe`、`MCServerDownloadTool-linux-x86_64` 和 `MCServerDownloadTool-macos-aarch64`；
- `release-index.json` 的 `tag`、`version`、`commit` 与发布 tag 一致；
- 每个 `.sha256` 的摘要可在干净环境对对应二进制复算通过；
- GitHub Attestations 页面能显示 `MCServerDownloadTool-*` 发布资产的 build provenance；
- 下载后的安装器在与 `server-install.json` 同目录时可不带 `--manifest` 启动；
- prerelease 版本没有被标记为 latest；
- macOS 发布说明没有声称已签名或已公证。

可使用 GitHub CLI 验证已下载资产的 attestation：

```bash
gh attestation verify MCServerDownloadTool-linux-x86_64 --repo OWNER/REPOSITORY
```

## 失败处理

发布流程在创建 Release 前完成所有构建和校验，因此前置 job 失败不会留下半成品 Release。修复原因后删除错误 tag，再在正确 commit 上创建新 tag；已经创建且被用户下载过的正式版本不得移动 tag 或覆盖资产，应递增版本重新发布。

workflow 不自动删除 Release 或 tag。任何删除都需要维护者明确操作并保留审计记录。
