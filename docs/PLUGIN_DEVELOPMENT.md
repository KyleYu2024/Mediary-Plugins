# Mediary 第三方插件开发指南

本文是第三方作者开发 Mediary 插件的公开接口说明。

- 适用宿主：Mediary 0.7.7 及以上
- 插件 API：v1
- 默认发布目标：Linux AMD64（x86_64 NAS / Docker）
- 插件形态：常驻进程、一次性动作、计划动作或 HTTP Webhook

插件是由 Mediary 托管的本机可执行程序，不是浏览器插件或动态链接库。插件进程拥有与
Mediary 相同的 OS 用户权限，scope 只限制它调用 Mediary HTTP API，不构成操作系统级沙箱。

## 1. 开发流程

1. 选择插件运行形态。
2. 创建 `plugin.json` 和入口程序。
3. 在本地 Mediary 的 `plugins/` 目录测试。
4. 为目标平台构建自包含程序。
5. 发布版本化 `.tar.gz` 和 SHA-256。
6. 按[商店上架指南](../CONTRIBUTING.md)提交 Pull Request。

最小可运行项目位于 [`examples/sample-action`](../examples/sample-action/)。正式实现可以参考
[`official`](../official/) 中的官方插件。

## 2. 插件目录

最小目录：

```text
sample-plugin/
├── plugin.json
└── plugin
```

运行后可能出现：

```text
sample-plugin/
├── plugin.json
├── plugin
├── config.json       # Mediary 创建和维护，不得放入分发包
└── data/             # 插件持久化数据目录，不得放入分发包
```

目录名必须与 `plugin.json` 的 `id` 完全一致。

## 3. 选择运行形态

| 能力 | 清单字段 | 适用场景 |
| --- | --- | --- |
| 常驻进程 | `runtime` | 长连接、持续同步、PT 刷流 |
| 一次性动作 | `action_runtime` | 搜索、查询、手动处理 |
| 计划动作 | `scheduled_actions` + `action_runtime` | 定时同步、榜单订阅 |
| 事件 Webhook | `events` + `webhook_url` | 接收订阅创建或删除事件 |
| 配置表单 | `settings_schema` | 在插件中心编辑设置 |
| 数据视图 | `data_view` | 展示插件生成的记录 |
| 交互视图 | `interactive_view` | 使用宿主提供的声明式操作界面 |

插件必须至少声明 `runtime`、`action_runtime` 或一个 `events` 项。定时任务应优先使用计划
动作，不要用常驻进程内部的无限循环和 `sleep` 模拟 cron。

## 4. `plugin.json`

一次性计划插件示例：

```json
{
  "api_version": 1,
  "id": "author.sample-plugin",
  "name": "示例插件",
  "version": "1.0.0",
  "description": "定时查询媒体目录并创建订阅",
  "action_runtime": {
    "entrypoint": "./plugin"
  },
  "scheduled_actions": [
    {
      "action": "refresh",
      "label": "立即刷新",
      "manual": true,
      "cron_setting": "cron",
      "timeout_seconds": 600
    }
  ],
  "requested_scopes": [
    "catalog:read",
    "subscriptions:read",
    "subscriptions:write"
  ],
  "settings_schema": {
    "sections": [
      {
        "title": "运行设置",
        "fields": [
          {
            "key": "$enabled",
            "type": "switch",
            "label": "启用插件",
            "default": false
          },
          {
            "key": "cron",
            "type": "text",
            "label": "执行周期",
            "default": "0 9 * * *"
          }
        ]
      }
    ]
  }
}
```

顶层字段：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `api_version` | 建议 | 当前只能是 `1`，省略时默认为 `1` |
| `id` | 是 | 唯一 ID，必须与目录名一致 |
| `name` | 是 | 插件中心显示名称 |
| `version` | 是 | SemVer 版本 |
| `description` | 否 | 简短功能说明 |
| `runtime` | 条件 | 常驻进程定义 |
| `action_runtime` | 条件 | 一次性动作进程定义 |
| `scheduled_actions` | 否 | 计划动作数组 |
| `requested_scopes` | 否 | 请求的 Mediary API 权限 |
| `auto_grant_scopes` | 否 | 是否自动授予声明权限，默认 `false` |
| `settings_schema` | 否 | 插件配置表单 |
| `secret_fields` | 否 | 需要在管理 API/UI 中脱敏的设置键 |
| `cookiecloud_domains` | 否 | `cookiecloud:read` 可读取的 Cookie 域名白名单；必须与该权限同时声明 |
| `data_view` | 否 | 数据文件展示定义 |
| `interactive_view` | 否 | 宿主渲染的交互视图 |
| `events` | 否 | Webhook 事件名，`"*"` 表示全部 |
| `webhook_url` | 否 | HTTPS 事件接收地址 |
| `signing_secret_field` | 否 | Webhook 签名密钥设置键 |

`id` 长度为 1 到 64，只能使用小写字母、数字、`.`、`_`、`-`，首字符必须是字母或数字，
并且不能包含 `..`。

`runtime` 和 `action_runtime` 使用相同格式：

```json
{
  "entrypoint": "./plugin",
  "args": ["--optional-argument"]
}
```

入口必须存在于插件目录内，不能使用绝对路径或 `..`。Linux/macOS 文件必须具有可执行权限。
宿主直接执行入口，不进行 shell 展开。

## 5. 运行环境

常驻进程和动作进程可以读取：

| 环境变量 | 含义 |
| --- | --- |
| `MEDIARY_PLUGIN_ID` | 当前插件 ID |
| `MEDIARY_PLUGIN_API_URL` | API 根地址，已经包含 `/api` |
| `MEDIARY_PLUGIN_TOKEN` | 插件专用 Bearer Token |
| `MEDIARY_PLUGIN_SETTINGS_JSON` | 用户设置 JSON |
| `MEDIARY_PLUGIN_DATA_DIR` | 持久化数据目录绝对路径 |
| `MEDIARY_PLUGIN_CONFIG_PATH` | Mediary 管理的 `config.json` 路径，只读 |
| `MEDIARY_PLUGIN_ACTION` | 当前动作名，仅动作进程 |
| `MEDIARY_PLUGIN_TRIGGER` | `manual` 或 `schedule`，仅动作进程 |
| `TZ` | 宿主时区 |
| `PATH` | `/usr/local/bin:/usr/bin:/bin` |

宿主会清空继承环境，因此不要依赖 `HOME`、宿主密钥或未列出的变量。不要保存、打印或转发
`MEDIARY_PLUGIN_TOKEN`。

### 5.1 常驻进程

- 工作目录是插件目录。
- 插件停用时不启动。
- 保存配置或权限后，宿主会停止旧进程并重新启动。
- 异常退出后按 `2、4、8、16、32、60` 秒退避重启。
- 正常以状态码 `0` 退出后不会自动重启。
- 进程应响应终止信号，并保证重复启动不会破坏数据。
- 标准输出和错误可用于日志，但不得包含凭据。

### 5.2 一次性动作

宿主启动动作后：

1. 请求 JSON 原样写入标准输入。
2. `MEDIARY_PLUGIN_ACTION` 提供动作名。
3. 程序向标准输出写入一个有效 JSON 值。
4. 成功时退出码为 `0`；失败时使用非零退出码并把简短错误写入标准错误。

限制：

- 普通动作最长 45 秒；
- 计划动作可声明 1 到 86400 秒超时；
- 标准输出最大 2 MB，且只能包含一个 JSON 值；
- 同一个计划动作不会并发执行。

成功响应可以包含面向用户的提示：

```json
{
  "notice": "刷新完成：处理 10 项，新增 2 项。",
  "report": {
    "processed": 10,
    "created": 2
  }
}
```

### 5.3 计划动作

```json
{
  "scheduled_actions": [
    {
      "action": "refresh",
      "label": "立即刷新",
      "manual": true,
      "cron_setting": "cron",
      "timeout_seconds": 600
    }
  ]
}
```

每项必须且只能声明固定 `cron` 或读取设置的 `cron_setting`。使用五段表达式
`分 时 日 月 周`，按宿主时区解释，例如：

| 表达式 | 含义 |
| --- | --- |
| `0 9 * * *` | 每天 09:00 |
| `*/30 * * * *` | 每 30 分钟 |
| `0 3 * * 1` | 每周一 03:00 |

停用或宿主离线期间错过的任务不会补跑。配置保存后会自动重新计算下次执行时间。

## 6. 配置和数据

`settings_schema.sections[].fields[]` 支持：

| 字段 | 说明 |
| --- | --- |
| `key` | 设置键；`$enabled` 是宿主保留键 |
| `type` | `switch`、`text`、`number`、`select`、`multiselect` |
| `label` / `help` / `placeholder` | 界面文本 |
| `default` | 首次发现插件时的默认值 |
| `min` / `max` / `step` | 数字约束 |
| `options` | `{ "label", "value" }` 数组 |
| `span` | `2` 表示宽布局 |

密码和 Token 对应的键必须加入 `secret_fields`。插件会在
`MEDIARY_PLUGIN_SETTINGS_JSON` 中收到真实值，因此不得记录完整设置 JSON。

插件数据只能写入 `MEDIARY_PLUGIN_DATA_DIR`。如需在插件中心展示 JSON：

```json
{
  "data_view": {
    "title": "处理记录",
    "file": "data/records.json",
    "summary": [
      {"key": "total", "label": "总数"}
    ],
    "fields": [
      {"key": "title", "label": "名称", "primary": true},
      {"key": "created_at", "label": "处理时间"}
    ]
  }
}
```

数据文件必须位于 `data/`，大小不超过 2 MB，建议先写临时文件再原子替换。

## 7. API、认证和权限

所有请求使用：

```http
Authorization: Bearer <MEDIARY_PLUGIN_TOKEN>
```

URL 基于 `MEDIARY_PLUGIN_API_URL` 拼接，不要写死主机、端口或再次添加 `/api`。插件只能访问
`requested_scopes` 声明且用户实际授予的 API。

专用插件 API：

| Scope | 路由 | 用途 |
| --- | --- | --- |
| `torrents:read` | `GET /plugin/sites` | 列出已启用 PT 站点，不返回凭据 |
| `torrents:read` | `GET /plugin/torrents` | 搜索或读取最新种子 |
| `downloads:create` | `POST /plugin/downloads` | 添加原始下载、磁力或 `mteam://` 任务 |
| `downloader:read` | `GET /plugin/downloader/torrents` | 读取下载器完整任务状态 |
| `cookiecloud:read` | `GET /plugin/cookiecloud?domain=<域名>` | 读取清单 `cookiecloud_domains` 允许域名及其子域可携带的 Cookie |

插件通过 `POST /plugin/notifications` 发送通知时可以传入 HTTPS `image_url`。未传、留空或
使用非 HTTPS 地址时，宿主会自动改用 Mediary 内置通知图。

`GET /plugin/torrents` 支持 `keyword`、逗号分隔的 `site_ids`、1 到 1000 的 `limit` 和
`free_only`。返回的种子项包括标题、站点、大小、下载 URL、人数、促销、Tracker 元数据和
Mediary 标题解析结果。

其他可授权 API：

| Scope | 允许的主要路由 |
| --- | --- |
| `dashboard:read` | `GET /dashboard/summary`、`GET /dashboard/emby` |
| `catalog:read` | `GET /search/tmdb`、`GET /search/tmdb/details`、`POST /tmdb/resolve` |
| `subscriptions:read` | `GET /subscriptions`、`GET /subscriptions/*` |
| `subscriptions:write` | `POST /subscriptions`、其他非 GET `/subscriptions/*` |
| `downloads:read` | `GET /downloads`、`GET /downloads/*`、下载/转移历史 |
| `downloads:write` | 非 GET `/downloads/*` |
| `sites:read` | `GET /sites`、`GET /sites/*` |
| `sites:write` | `POST /sites`、其他非 GET `/sites/*` |
| `filters:read` | `GET /filter/*` |
| `filters:write` | 非 GET `/filter/*` |
| `logs:read` | `GET /logs` |
| `integrations:read` | `GET /open115/*`、`GET /flowlink/*`、`GET /link/*` |
| `integrations:run` | 非 GET `/open115/*`、`/flowlink/*`、`/link/*` |
| `settings:read` | `GET /settings` |

优先使用权限更窄的专用 API。插件应忽略未知响应字段，并正确处理 `401`、`4xx`、`5xx`、
网络超时和空结果。

常见订阅流程：

1. 用 `POST /tmdb/resolve` 和 `catalog:read` 解析标题。
2. 用 `GET /subscriptions` 和 `subscriptions:read` 检查重复。
3. 用 `POST /subscriptions` 和 `subscriptions:write` 创建订阅。

## 8. Webhook

v1 公开：

| 事件 | 数据 |
| --- | --- |
| `subscription.created` | `id`、`title`、`tmdb_id`、`media_type`、`year`、`season` |
| `subscription.deleted` | `id` |

请求包含：

```text
X-Mediary-Plugin-Id: <plugin-id>
X-Mediary-Event: subscription.created
X-Mediary-Event-Id: <event-id>
X-Mediary-Timestamp: <Unix秒>
X-Mediary-Signature: sha256=<hex>
```

签名算法：

```text
signed_payload = timestamp + "." + raw_request_body
signature = hex(HMAC-SHA256(signing_secret, signed_payload))
```

接收端必须使用原始请求字节和常量时间比较验签，检查时间偏差，按事件 ID 去重并保证幂等。
请求超时 15 秒；网络错误和 `5xx` 最多投递 3 次，`4xx` 不重试。

## 9. 声明式交互视图

v1 不允许插件注入 HTML、JavaScript 或 CSS。`interactive_view` 可以声明由宿主渲染的表单，
并通过 `action_runtime` 返回结构化 `items`。结果项支持：

- 标题、副标题、徽标和元数据；
- `copy`：复制文本；
- `link_submit`：把链接交给 Mediary 集成处理；
- `plugin_action`：调用当前插件的另一个动作；
- `subscription_create`：打开 Mediary 订阅创建窗口。

字段和输出格式可直接参考
[`official/pansou/plugin.json`](../official/pansou/plugin.json)及其
[`实现`](../official/src/bin/pansou-plugin.rs)。

## 10. 构建和打包

官方 Docker 基于 Debian 12 `bookworm-slim`，仅保证存在 `ca-certificates`、`curl`、
`libssl3`、`tzdata` 和基础 shell。不要假定存在 Python、Node.js、Java 或 Rust 工具链。

NAS 默认目标是 Linux AMD64。建议构建自包含的 `x86_64` 可执行文件，并在 Debian 12 环境
验证动态链接依赖。不同 OS/CPU 必须分别构建。

推荐产物：

```text
author.sample-plugin-1.0.0-linux-amd64.tar.gz
SHA256SUMS
```

打包：

```bash
chmod 755 plugin
tar -czf author.sample-plugin-1.0.0-linux-amd64.tar.gz plugin plugin.json
sha256sum author.sample-plugin-1.0.0-linux-amd64.tar.gz > SHA256SUMS
```

macOS 计算哈希可以使用：

```bash
shasum -a 256 author.sample-plugin-1.0.0-linux-amd64.tar.gz
```

压缩包根目录直接包含 `plugin` 和 `plugin.json`，不得包含 `config.json`、`data/`、凭据、
符号链接、硬链接或本机路径。商店安装包上限为 100 MB。

## 11. 本地安装和测试

把插件目录放到：

```text
<config-dir>/plugins/<plugin-id>/
```

Docker 容器内通常是：

```text
/app/config/plugins/<plugin-id>/
```

重启 Mediary 后进入“插件中心”，完成配置、授权和启用。至少测试：

- 首次发现、默认停用和权限授予；
- 正常动作、空结果、网络超时和非零退出；
- 停用、重新启用和配置热重载；
- 重复运行与写操作幂等；
- 升级后保留 `config.json` 与 `data/`；
- 卸载后停止运行任务；
- 日志和错误中没有凭据。

## 12. 发布与商店上架

第三方作者在自己的公开仓库维护源码、CI 和 GitHub Release。本仓库不代替作者构建插件。

每次 Release 应提供：

- 版本化 `.tar.gz`；
- 对应 SHA-256；
- 支持平台和最低 Mediary 版本；
- 变更日志；
- 新增权限、遥测、外部服务和数据收集披露。

首次上架和版本更新的具体步骤见[商店上架指南](../CONTRIBUTING.md)。商店索引的
`permissions` 必须与包内 `requested_scopes` 完全一致。

## 13. 安全与兼容性

- 只申请核心功能必需的最小 scope。
- 不读取 Mediary 数据库、站点 Cookie、下载器密码或主 API Token。
- 不扫描插件目录之外的文件。
- 不下载并执行未经校验的远程代码。
- 不向未披露的服务上传媒体、日志、配置或站点信息。
- 对网络请求设置超时，对写操作做幂等和去重。
- 只写 `MEDIARY_PLUGIN_DATA_DIR`，不修改 `config.json`。
- 忽略 API 中新增的未知字段，不依赖 JSON 对象字段顺序。
- 升级时兼容旧配置和旧数据，必要时显式迁移。
- 在版本说明中披露新增权限和破坏性变更。

## 14. 发布前清单

- [ ] `api_version` 为 `1`
- [ ] 目录、清单和索引中的插件 ID 一致
- [ ] 版本使用 SemVer
- [ ] 入口路径安全且 Linux 文件具有执行权限
- [ ] 在声明的平台和最低宿主版本测试
- [ ] 只申请必要权限
- [ ] 秘密设置已加入 `secret_fields`
- [ ] 日志、错误和分发包没有凭据
- [ ] 动作输出是 2 MB 内的单个 JSON 值
- [ ] 计划任务的 cron、超时和防并发已验证
- [ ] 数据文件通过原子写入更新且不超过 2 MB
- [ ] Webhook 已实现验签、去重和幂等
- [ ] 分发包不含 `config.json`、`data/` 或链接
- [ ] Release 提供版本说明、架构和 SHA-256
- [ ] 索引权限与包内清单一致

## 15. 当前 v1 限制

- 商店不会后台自动更新插件。
- 新安装或替换入口文件后需要重启 Mediary。
- 没有插件依赖和服务发现协议。
- 没有任意前端代码扩展协议。
- 计划任务不会补跑离线期间错过的执行。
- 数据视图只支持一个不超过 2 MB 的本地 JSON 文件。
- 官方 Docker 不提供 Python、Node.js 等脚本运行时。
- 当前业务 Webhook 只有订阅创建和删除。

需要未公开能力时，请先提交 Issue 讨论接口，不要依赖 Mediary 的内部数据库或前端实现。
