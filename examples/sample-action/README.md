# 最小动作插件

这个示例只依赖 POSIX shell，用于验证 Mediary 动作协议。安装后可以从插件卡片点击“打开”，
也可以在终端中直接测试。它读取标准输入，并向标准输出写入一个 JSON 响应。

本地测试：

```bash
printf '{"name":"Mediary"}' | MEDIARY_PLUGIN_ACTION=hello ./plugin
```

预期输出：

```json
{"notice":"示例动作执行完成","received":{"name":"Mediary"}}
```

开始开发时复制整个目录，至少修改：

- 目录名和 `plugin.json` 的 `id`；
- 名称、版本和说明；
- `requested_scopes`；
- `action_runtime` 和程序实现。

完整协议见[插件开发指南](../../docs/PLUGIN_DEVELOPMENT.md)。
