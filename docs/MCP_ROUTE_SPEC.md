## Mediary 核心 `/mcp` 路由规格

### 核心侧（Mediary 后端）

| 责任 | 细节 |
|------|------|
| 路由 | `POST /mcp` |
| 认证 | 仅接受 `Authorization: Bearer <主 API Token>`，不接受 JWT、插件 Token、查询参数 Token |
| 插件声明检查 | 插件必须在 `plugin.json` 声明 `external_actions: ["mcp"]` |
| 启用状态检查 | 插件未启用时返回 `503` |
| 请求体限制 | 512 KB |
| stdout 限制 | 流式限制 2 MB |
| stderr 限制 | 64 KB，脱敏后返回 |
| 执行超时 | 启动 → 写 stdin → 读输出 → 等待退出，统一 60 秒，超时返回 `504` 并终止进程 |
| 并发限制 | 最多 4 个并发，超限返回 `429` |
| stdin | 写入 MCP 请求 body |
| stdout | 解析为 JSON 返回客户端；为空时返回 `202 Accepted`（通知类请求） |
| 环境变量 | `MEDIARY_PLUGIN_ACTION=mcp` |

| 错误码 | 场景 |
|--------|------|
| `401` | 无 Token 或无效 Token |
| `429` | 并发超过上限 |
| `500` | 宿主管道读写失败 |
| `502` | 插件非零退出 / 非法 JSON / 超 2MB / stdout 无法解析为 JSON |
| `202` | 插件 exit 0 且 stdout 为空（MCP 通知类请求的正常行为） |
| `503` | 插件未启用或未声明 `external_actions` |
| `504` | 执行超时 60 秒 |

### 插件侧（mcp-server）

| 责任 | 细节 |
|------|------|
| MEDIARY_PLUGIN_ACTION=mcp | 处理 JSON-RPC 2.0 请求 |
| 输出 | stdout 仅输出一个 JSON-RPC 响应 |
| 通知 | `id: null` 的请求执行后不输出，直接 exit 0 |
| 日志 | 全部写 stderr |
| 方法 | `initialize`、`tools/list`、`tools/call`、`ping`、`notifications/initialized` |
| 退出时间 | 60 秒内 |
| 边界 | 不自行监听端口 |

### 客户端配置示例

```json
{
  "mcpServers": {
    "mediary": {
      "type": "http",
      "url": "http://<Mediary地址>/mcp",
      "headers": {
        "Authorization": "Bearer <Mediary API Token>"
      }
    }
  }
}
```
