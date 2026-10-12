# Mediary Plugins

Mediary 官方插件开发者入口与公开商店索引。插件作者可以在这里完成从开发、测试、打包发布
到提交商店审核和后续版本更新的完整流程。

## 开发者入口

| 我想做什么 | 从这里开始 |
| --- | --- |
| 第一次开发 Mediary 插件 | [插件开发指南](docs/PLUGIN_DEVELOPMENT.md) |
| 查看最小可运行插件 | [示例插件](examples/sample-action/) |
| 查看正式插件实现 | [官方插件源码](official/) |
| 提交新插件或版本更新 | [商店上架指南](CONTRIBUTING.md) |
| 填写商店索引条目 | [索引条目模板](templates/plugin.json) |
| 查看索引字段约束 | [索引 JSON Schema](schema/plugin.schema.json) |
| 报告安全问题 | [安全政策](SECURITY.md) |

第一次接入建议按以下顺序进行：

1. 阅读[插件开发指南](docs/PLUGIN_DEVELOPMENT.md)，确定使用常驻进程、一次性动作、计划动作
   还是 Webhook。
2. 复制[最小示例](examples/sample-action/)，修改插件 ID、清单和程序。
3. 在 Linux AMD64 的 Mediary 环境中完成安装、权限和异常场景测试。
4. 在自己的 GitHub 仓库创建版本化 Release，并上传 `.tar.gz` 和 SHA-256。
5. 按[商店上架指南](CONTRIBUTING.md)提交 Pull Request。

## 商店工作方式

Mediary 插件中心读取本仓库经过审核的 `catalog.json`。第三方插件的源码、构建流程和 Release
仍由各作者在自己的仓库维护，本仓库只保存版本、兼容性、权限、下载地址和校验值。

```text
作者仓库与 Release
        │
        ▼
plugins/<plugin-id>.json
        │
        ▼
catalog.json
        │
        ▼
Mediary 插件中心
```

官方索引地址：

```text
https://raw.githubusercontent.com/KyleYu2024/Mediary-Plugins/main/catalog.json
```

Mediary 官方插件源码镜像位于 `official/`，由 Mediary 主仓库的插件 tag 同步。本仓库会在
公开 GitHub Actions 中构建 Linux/macOS 的 AMD64 与 ARM64 安装包、发布 Release，并以
实际产物的 SHA-256 更新官方条目。

## 责任边界

- 插件作者负责源码、二进制、Release、版本维护和用户支持。
- 维护者负责索引格式检查、基本安全审核、风险标记和下架。
- 插件是与 Mediary 同一 OS 用户运行的可执行程序，不受操作系统级沙箱隔离。
- 索引收录不代表 Mediary 对第三方插件源码、服务可用性或行为作担保。
- 仓库不接收密码、Token、Cookie、私钥或其他敏感信息。

## 许可证

仓库中的校验脚本和文档使用 MIT License。各插件沿用其作者声明的许可证。
