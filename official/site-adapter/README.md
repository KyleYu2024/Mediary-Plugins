# 站点适配管理插件（已下架）

新版 Mediary 已内置“站点管理 → 自定义站点”，无需另行安装本插件。官方商店不再分发，
后续全体插件发布也不会重新生成本插件索引。旧源码和历史 Release 保留供兼容。

请先更新到包含内置自定义站点功能的 Mediary，再进入该入口检查迁移后的规则并重新启用。
原规则、本机参数和回退版本会复制到主程序配置目录 `custom-sites/`，不会自动启用；
内置启用规则优先。确认迁移无误后，可停用或卸载旧插件，内置副本不受影响。
也可从旧插件导出 `.mediary-site.json`，在内置入口手动导入（本机参数需另填）。

面向用户的完整教程与 AI 提示词见 [站点适配指南](GUIDE.md)。以下本机命令在 Mediary 主仓库根目录执行。

这是一个不附带任何站点规则的运行容器。用户自行编写或导入 `.mediary-site.json`
适配包，启用后由它优先处理对应站点。可以导出规则供其他用户导入；主程序不必随
每次站点改版升级，插件本身也不必随规则变化更新。

需要 Mediary 3.1.6 及以上，具有 `site_adapter` v1 接口和“站点适配管理”界面。旧版主程序安装插件
不能自动获得这些接入能力。后续超出 v1 契约的新能力仍可能需要升级宿主或插件。

## 使用

1. 安装插件，在配置中授予“接管站点抓取与下载”权限。
2. 打开“管理适配包”，导入 JSON 文件或粘贴包内容。也可点“新建模板”自行编写。
3. 检查包声明的域名和操作，填写包需要的本机参数，然后启用。
4. 选择主程序中已配置的站点，执行资源预览。
5. 按平常方式使用 Mediary 的搜索和订阅功能。

新导入的规则默认停用；替换规则后同样需要重新启用。保留一份旧规则，可以回退。
同一域名和操作不能同时启用两份适配。已接管的操作失败时返回错误；未接管的操作
仍使用主程序内置方式。停用插件、撤销权限或停用适配包即可恢复内置处理。

站点登录、CookieCloud 同步、订阅调度、匹配筛选、下载器管理均由主程序负责。
插件只读取当前站点的信息并获取种子，不持有数据库或主程序管理员凭据。
`profile` 是使用现有凭据读取站点资料，不是登录协议；没有 SRP、密码登录或 Cookie 同步操作。

导出只包含包定义，不包含本机参数或主程序的 Cookie、API Key、RSS 地址等。
不要把个人凭据硬编码进脚本或包说明；手动写入脚本的内容也会被导出。

## 开发和本机安装

插件是独立 Rust 可执行程序，内嵌 Rhai 脚本引擎，不依赖 NAS 安装 Node.js 或 Python。

```bash
cargo build --manifest-path plugins/site-adapter/Cargo.toml --locked
python3 tools/install-site-adapter.py
```

安装工具只复制本机编译产物和清单到本机配置目录，不构建 Docker，不部署 NAS。
其他平台分发时将该平台的 `site-adapter-plugin` 重命名为 `plugin`，与 `plugin.json`
放在 `site-adapter/` 目录下。

隔离验证（本机模拟 HTTP 站点，不接触真实站点）：

```bash
python3 tools/test-site-adapter.py
```

## 适配包 v1

```json
{
  "format_version": 1,
  "id": "example-site",
  "name": "示例站点",
  "version": "1.0.0",
  "description": "根据实际接口编写",
  "domains": ["example.invalid"],
  "network_domains": [],
  "operations": ["latest", "search", "download"],
  "settings_fields": [{"key":"custom_parameter","label":"本机参数","secret":true}],
  "script": "fn latest(ctx) { #{items: []} } fn search(ctx) { latest(ctx) } fn download(ctx) { throw \"请实现下载接口\"; }"
}
```

- `domains` 是精确站点主机名列表，不支持通配符、端口和路径。
- `network_domains` 是脚本可以额外请求的主机名，例如 API 子域。跨域请求不会自动带上站点凭据。
- 可选 `credential_domains` 声明额外允许携带本站凭据的域名，必须已在上述域名清单中，
  且仅允许 HTTPS 443。用于 API 与网页分属不同主机的站点。界面会明确显示这些域名。
- `operations` 可选 `latest/search/rss/download/profile/messages`。
- 包含资源抓取操作时必须同时实现 `download`，避免产生无法下载的资源。
- `settings_fields` 只声明字段，不包含默认值或个人值。本机填写的值单独保存。
- `script` 为 Rhai 源码，各操作是一个同名函数，参数仅有 `ctx`，返回对象。
- 限制：最多 64 份包、脚本 512 KB、单次 20000 条资源、32 次 HTTP 请求。

Rhai 语法和嵌入接口参考：[官方文档](https://rhai.rs/book/)。它不是 JavaScript。

### ctx

```text
ctx.site.id / name / url / rss_url
ctx.input.keyword / max_pages / search_area   搜索时提供
ctx.input.torrent_id                         下载时提供，字符串
ctx.settings.<key>                           本机参数
```

脚本无法直接获取 Cookie、Authorization、API Key；HTTP 帮助函数在请求与站点同源时
自动注入。个人 RSS 地址和本机参数可能含秘密，不要将其返回到资源字段或异常中。

### 资源输出

`latest(ctx)`、`search(ctx)`、`rss(ctx)` 返回 `#{items: [...]}`。
分页、关键词查询和站点数据转换由脚本处理；`max_pages` 为建议的抓取页数上限，
值为 unit（JSON null）时表示主程序未限制页数。主程序提供超时和请求次数限制。
如果返回的是全量目录，搜索脚本需要自行按关键词过滤。

每个 item：

| 字段 | 类型 | 说明 |
|---|---|---|
| id | string，必填 | 稳定资源 ID；不放会轮换的下载 token |
| title | string，必填 | 种子标题 |
| size | integer | 字节数，默认 0，不能为负 |
| page_url | string，可选 | HTTP(S) 详情页，支持相对路径 |
| description | string，可选 | 副标题或描述 |
| seeders / leechers | integer | 做种和下载人数，默认 0 |
| grabs | integer，可选 | 完成次数 |
| publish_time / freedate | string，可选 | RFC3339、RFC822 或 UTC `YYYY-MM-DD HH:MM:SS` |
| category / uploader / imdbid | string，可选 | 分类、发布者、IMDb ID |
| labels | string[] | 标签 |
| uploadvolumefactor / downloadvolumefactor | number，可选 | 实际倍率，如免费为下载倍率 0 |
| hit_and_run | bool | H&R 标记 |

未知促销倍率应省略；不要猜测枚举，不把未知状态标为免费。
主程序仍负责从标题识别片目、年份、季集、清晰度等，适配包不定义订阅逻辑。

### 下载输出

`download(ctx)` 返回 `#{torrent_base64: torrent_get(url)}`。
主程序保存稳定的站点、包、资源 ID。真正下载和读取种子文件清单时，主程序使用
最新同步的凭据调用这个函数。脚本可以先请求详情或签发接口，再获取种子文件。
最多返回 10 MB 种子，不支持在脚本中直接提交下载器任务。

停用或删除原适配后，旧适配资源的下载会明确报错，需要重新抓取；不会静默使用
其他包下载同名 ID。更新包时应保持 `id` 和资源 ID 的含义稳定。

### 资料和站内信

`profile(ctx)` 返回：

```text
#{profile: #{user_level: "User", ratio: 1.5, uploaded: 1000000,
             downloaded: 666666, message_unread: 0}}
```

这些字段需要全部提供，单位为字节。资料不完整时不要声明 profile，避免将未知
流量当作零覆盖主程序数据。累计流量与月度流量不能直接互换。

`messages(ctx)` 返回：

```text
#{messages: [#{key: "稳定消息 ID", title: "标题", date: "时间",
              content: "正文", url: "站内信详情 HTTP(S) 地址"}]}
```

仅声明能够无副作用读取未读消息的实现；预览界面不调用 messages。

### 脚本帮助函数

| 函数 | 返回值 |
|---|---|
| http_get(path_or_url) | `#{status, body, json}`，JSON 无法解析时 json 为 unit |
| http(#{url, method, headers, body, auth, encoding}) | 同上；GET/POST/HEAD，body 可为字符串或 JSON 对象；encoding 可指定 gbk 等编码 |
| torrent_get(path_or_url) | 种子文件的标准 Base64 字符串 |
| absolute_url(path) | 基于当前站点的完整 HTTP(S) 地址 |
| url_encode(text) | URL 查询/路径参数编码 |
| json_parse(text) / json_stringify(value) | JSON 与 Rhai 数据互转 |
| html_select(html, css) | `[{text, html, attrs}]`，可继续选择返回的 html |
| rss_items(xml) | RSS item 数组，包含 title/guid/page_url/download_url/size/publish_time/description |

`auth` 默认 `all`，可设为 `cookie`、`authorization`、`api_key` 或 `none`；不允许脚本自己设置 Cookie、Authorization、
X-API-Key 请求头。跨域只在额外声明的凭据域名注入这些凭据。重定向也需要符合声明域名，且每次重新
判断是否同源。适配必须明确允许所需 API 域名。

响应最多 16 MB、种子最多 10 MB。HTTP 错误返回 status，由脚本判断；下载帮助函数会
拒绝非成功响应。脚本有操作次数、调用栈和执行时间限制，不开放文件、进程或模块加载。
脚本异常内容隐藏，仅显示错误位置，避免将接口正文和凭据写进日志。

## 宿主接入

`plugin.json` 声明 `site_adapter: {action, timeout_seconds}` 与 `sites:adapt` 权限。
主程序通过 stdin 发送 v1 契约，仅包含当前操作对应的站点信息和凭据。响应包含
`handled: false` 表示不接管；接管则包含 `handled: true`、`adapter_id` 与上述业务输出。
第三方适配管理插件也可以实现相同契约。

原生插件本身仍是受信任进程；上述脚本能力限制由本插件的 Rhai 运行环境执行，
不表示现有原生插件权限系统是操作系统沙箱。
