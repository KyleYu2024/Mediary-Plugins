# Mediary 插件商店上架指南

本文说明第三方作者如何首次上架插件、发布更新以及通过商店审核。插件接口和运行协议请先阅读
[插件开发指南](docs/PLUGIN_DEVELOPMENT.md)。

## 1. 上架前准备

作者需要有一个公开的源码仓库，并为准备上架的版本创建不可变的 Release。Release 至少包含：

- 一个目标平台的 `.tar.gz` 安装包；
- 安装包对应的小写 64 位 SHA-256；
- 版本说明、安装要求和兼容的最低 Mediary 版本；
- 新增权限、遥测、外部服务或数据收集行为的披露。

Mediary 主要运行在 NAS Docker 环境中，因此默认和优先验收 `linux-amd64`。其他平台可以
按需提供：

| 平台键 | 目标环境 |
| --- | --- |
| `linux-amd64` | x86_64 Linux/NAS，推荐默认提供 |
| `linux-arm64` | ARM64 Linux/NAS |
| `darwin-amd64` | Intel macOS |
| `darwin-arm64` | Apple Silicon macOS |

至少提供一个平台。每个平台必须使用独立安装包和独立 SHA-256。

## 2. 安装包要求

推荐文件名：

```text
<plugin-id>-<version>-linux-amd64.tar.gz
<plugin-id>-<version>-linux-arm64.tar.gz
SHA256SUMS
```

压缩包根目录必须直接包含 `plugin.json` 和清单声明的入口文件：

```text
plugin.json
plugin
```

安装包不得包含：

- `config.json` 或 `data/`；
- 密码、Token、Cookie、私钥或测试凭据；
- 符号链接、硬链接或指向包外的路径；
- 构建缓存、`.git/` 或本机绝对路径；
- 运行时下载并执行的未声明代码。

插件 ID 必须与包内 `plugin.json` 的 `id`、安装目录名和索引文件名完全一致。版本必须使用
SemVer，例如 `1.2.0`。下载地址必须是不可变的版本化 HTTPS 地址，不能使用会被覆盖的
`latest` 文件。

## 3. 首次上架

1. Fork 本仓库并创建分支。
2. 复制 `templates/plugin.json` 为 `plugins/<plugin-id>.json`。
3. 填写真实版本、作者、权限、兼容版本和各平台 Release 信息。
4. 运行索引生成与文档校验。
5. 提交 `plugins/<plugin-id>.json` 和生成后的 `catalog.json`。
6. 创建 Pull Request，并完整填写 PR 模板。

```bash
cp templates/plugin.json plugins/<plugin-id>.json
node scripts/build-catalog.mjs
node scripts/check-doc-links.mjs
git diff --exit-code catalog.json
```

索引条目示例：

```json
{
  "id": "author.plugin-name",
  "name": "插件名称",
  "version": "1.0.0",
  "description": "一句话说明插件用途",
  "author": "作者名称",
  "homepage": "https://github.com/author/plugin-name",
  "source": "https://github.com/author/plugin-name",
  "license": "MIT",
  "api_version": 1,
  "min_mediary_version": "0.8.0",
  "permissions": ["catalog:read"],
  "artifacts": {
    "linux-amd64": {
      "url": "https://github.com/author/plugin-name/releases/download/v1.0.0/plugin-linux-amd64.tar.gz",
      "sha256": "64位小写SHA-256",
      "size": 123456
    }
  }
}
```

`permissions` 必须与安装包内 `plugin.json` 的 `requested_scopes` 完全一致。`size` 可选，
单位为字节，建议填写。

## 4. 发布插件更新

不要覆盖已经上架的 Release 文件。更新流程如下：

1. 在插件源码和包内 `plugin.json` 中更新版本。
2. 创建新的 Git tag 和 GitHub Release。
3. 上传新的版本化安装包并重新计算 SHA-256。
4. 修改原有 `plugins/<plugin-id>.json` 的版本、下载地址、哈希和大小。
5. 如有需要，更新 `min_mediary_version` 和 `permissions`。
6. 运行校验并提交新的 Pull Request。

新增权限、外部服务、遥测、数据上传或破坏性配置迁移时，必须在 Release Notes 和 PR 中同时
说明。仅修复二进制但不更新版本和下载地址的提交不会被接受。

## 5. 本地校验

提交前执行：

```bash
node scripts/build-catalog.mjs
node scripts/check-doc-links.mjs
git diff --exit-code catalog.json
```

还应手动确认：

- Release 下载地址无需登录即可访问；
- SHA-256 来自本次 Release 的最终文件；
- 压缩包根目录结构正确；
- Linux 入口具有可执行权限；
- `plugin.json`、索引权限和版本完全一致；
- 在声明的平台和最低 Mediary 版本上完成测试；
- 安装、配置、启用、停用、更新和卸载流程正常。

## 6. 审核内容

维护者会检查：

- 清单和索引格式；
- 下载目标、SHA-256、文件大小与包结构；
- 权限是否符合最小授权原则；
- 外部网络、遥测和数据收集是否充分披露；
- 基本功能、兼容性和失败处理；
- 源码仓库、Release 与提交作者之间的可追溯性。

审核不能替代操作系统级沙箱。插件存在恶意行为、供应链风险、失效下载、隐瞒数据收集或长期
不兼容时，维护者可以拒绝、标记或删除条目。

## 7. 安全问题与下架

普通兼容问题请提交 Issue。涉及漏洞或尚未公开的供应链风险时，按照
[安全政策](SECURITY.md)私下报告，不要在公开 Issue 中粘贴凭据或利用细节。

紧急下架请求应提供插件 ID、受影响版本、平台和可复现证据。下架只会从索引中移除条目，
不会远程删除用户已经安装的插件。
