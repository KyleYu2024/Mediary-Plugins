# Mediary Plugins

Mediary 官方插件索引。Mediary 插件中心从本仓库读取经过审核的插件元数据，
插件包仍由各作者通过自己的 GitHub Releases 或其他 HTTPS 地址发布。

Mediary 官方插件源码镜像位于 `official/`，由 Mediary 私有主仓库的插件 tag 同步。本仓库
会在公开 GitHub Actions 中构建 Linux/macOS 的 AMD64 与 ARM64 安装包、发布 Release，
再以实际产物的 SHA-256 自动更新官方条目。

## 上架插件

1. 在自己的仓库发布插件包。压缩包根目录必须包含 `plugin.json` 和清单中声明的可执行文件。
2. 为每个平台生成 SHA-256。
3. 复制 `templates/plugin.json` 到 `plugins/<plugin-id>.json` 并填写真实信息。
4. 运行 `node scripts/build-catalog.mjs` 更新 `catalog.json`。
5. 提交 Pull Request，并在说明中列出测试过的 Mediary 版本和平台。

详细要求参见 [CONTRIBUTING.md](CONTRIBUTING.md)。索引中的条目不代表 Mediary
对第三方插件源码或行为作担保，安装前仍应核对作者、权限和来源。

## 索引地址

```text
https://raw.githubusercontent.com/KyleYu2024/Mediary-Plugins/main/catalog.json
```

## 仓库职责

- 本仓库保存插件介绍、版本、兼容性、下载地址和 SHA-256。
- 插件作者负责源码、二进制、Release 和版本维护。
- 维护者负责格式检查、基本安全审核、风险标记和下架。
- 不接收密码、Token、Cookie、私钥或其他敏感信息。

## 许可证

仓库中的校验脚本和文档使用 MIT License。各插件沿用其作者声明的许可证。
