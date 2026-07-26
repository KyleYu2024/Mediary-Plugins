# 插件上架规则

## 必要条件

- 插件 ID 必须与 `plugin.json` 以及安装目录名一致。
- 版本使用 SemVer，例如 `1.2.0`。
- 下载地址必须使用 HTTPS，且应指向不可变的版本化发布文件。
- 每个平台的发布文件必须提供小写的 64 位 SHA-256。
- 插件包解压后的根目录必须直接包含 `plugin.json`。
- 插件不得静默收集凭据、绕过 Mediary 权限或下载并执行未声明的代码。
- 新增权限、遥测、外部服务或数据收集行为必须在说明中明确列出。

## 支持的平台键

- `linux-amd64`
- `linux-arm64`
- `darwin-amd64`
- `darwin-arm64`

至少提供一个平台。运行在 Docker 中的 Mediary 通常使用 `linux-amd64` 或
`linux-arm64`。

## 审核

维护者会检查清单、下载目标、哈希、权限说明和基本行为。审核不能替代操作系统级沙箱；
Mediary 插件是本机可执行程序，只应安装可信作者发布的插件。

插件存在恶意行为、供应链风险、失效下载或长期不兼容时，维护者可拒绝、标记或删除条目。
紧急下架应在 Issue 中提供插件 ID、受影响版本和可复现证据，但不要公开敏感数据。

## 本地校验

```bash
node scripts/build-catalog.mjs
git diff --exit-code catalog.json
```
