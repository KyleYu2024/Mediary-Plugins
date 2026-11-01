# 用插件适配和分享 PT 站点

## 1. 这套功能做什么

“站点适配管理”插件是一个空壳，安装后没有任何站点规则。用户可以为自己的站点编写
适配包，导入并启用，再导出给其他用户使用。站点改版后更新适配包即可，不需要逐次
升级 Mediary 或这个插件。

首次使用需要 Mediary 3.1.6 及以上，已接入站点适配契约 v1。今后的规则必须在当前插件
和契约支持的能力范围内。插件运行 Rhai 脚本，不运行 JavaScript、Python 或 shell。

| 环节 | 负责方 |
|---|---|
| 站点配置、登录状态、CookieCloud 同步 | Mediary 主程序 |
| 请求站点、解析目录、获取种子信息和种子文件 | 用户导入的适配脚本 |
| 订阅调度、片目匹配、规则筛选、去重、下载器管理 | Mediary 主程序 |
| 适配包导入导出、启停、回退和脚本运行 | 站点适配管理插件 |

适配包可以只接管需要修改的功能。未声明的功能继续使用内置处理；已接管的功能
执行失败会报错，方便排查。停用适配即可恢复内置方式。

## 2. 导入别人分享的适配包

1. 在主程序中添加站点，按原来的方式填写或同步 Cookie 等凭据。
2. 安装“站点适配管理”，在插件配置中授予站点适配权限。
3. 打开“管理适配包”，点击“导入文件”或“粘贴适配包”。文件为 JSON，建议后缀
   `.mediary-site.json`。
4. 检查名称、版本、站点域名、额外请求域名、允许携带凭据的域名和脚本内容。
5. 如有“本机配置”，填写自己实例需要的参数。适配包不应带作者的个人凭据。
6. 点击“启用”。选择自己配置的站点，执行“最新资源”或“搜索”预览。
7. 结果正确后，继续使用 Mediary 原来的搜索和订阅入口。

预览最多展示 20 条结果及匹配总数，不创建订阅、不向下载器提交任务，也不提供种子下载按钮。
适配的下载函数会在主程序实际需要种子时被调用。

导入新版本会停用该适配，保留本机参数和上一份规则。检查后重新启用；出现问题可点击
“回退”，再启用旧规则。同一域名、同一功能不能同时启用两份适配包。

## 3. 自己编写适配包

先取得有账号用户浏览器中的真实请求和响应，确认以下信息：

- 列表、搜索、分页、分类的请求方式和响应结构；最新资源如何排序。
- 标题、稳定 ID、大小、做种数、促销、H&R、发布时间等字段的真实含义。
- 列表中的任意资源如何取得种子文件，包括不在 RSS 中的旧种子。
- 主程序现有 Cookie、Authorization 或 API Key 是否足以请求接口。

只看到网页 JS 中的字段名，不能证明登录后的真实 JSON 层级和下载链路。
不清楚的能力可以暂不声明，但声明了资源抓取就必须实现下载。

### 3.1 适配包结构

```json
{
  "format_version": 1,
  "id": "my-site",
  "name": "我的站点",
  "version": "1.0.0",
  "description": "适配的接口和范围",
  "domains": ["example.invalid"],
  "network_domains": [],
  "credential_domains": [],
  "operations": ["latest", "search", "download"],
  "settings_fields": [],
  "script": "这里放完整 Rhai 源码"
}
```

| 字段 | 说明 |
|---|---|
| format_version | 当前为 1 |
| id | 稳定的包标识；小写字母、数字、`-`、`_`，最长 64 字符 |
| name / description | 用户可读名称和适配范围 |
| version | SemVer，例如 `1.0.1` |
| domains | 精确匹配的站点主机名；不写协议、端口和路径 |
| network_domains | 允许脚本额外请求的主机名，如 API 子域 |
| credential_domains | 额外允许自动注入本站凭据的主机名；须在上述域名清单中，且为 HTTPS 443 |
| operations | 声明支持的操作；每项必须有同名函数 |
| settings_fields | 本机参数的声明，包含 key、label、secret；不放个人值 |
| script | Rhai 源码字符串 |

从界面“新建模板”开始，可以直接编辑完整 JSON。“编辑”会打开现有包的完整内容。
可以改名并使用新 ID 建立另一份适配；更新原包时请保留 ID。

### 3.2 可实现的函数

```text
fn latest(ctx)    // 最新资源，返回 #{items: [...]}
fn search(ctx)    // 按 ctx.input.keyword 搜索，返回 #{items: [...]}
fn rss(ctx)       // 可选 RSS 适配，返回 #{items: [...]}
fn download(ctx)  // 按 ctx.input.torrent_id 取种子，返回 #{torrent_base64: ...}
fn profile(ctx)   // 可选：读取完整用户资料，返回 #{profile: ...}
fn messages(ctx)  // 可选：读取未读站内信，返回 #{messages: [...]}
```

不要在函数中实现订阅选择、创建下载任务或登录同步。

### 3.3 输入上下文

`ctx.site` 提供当前站点的 `id`、`name`、`url`、`rss_url`。资料检查时可能没有 id 和 RSS。
`ctx.input` 提供当前操作参数：搜索有 `keyword/max_pages/search_area`，下载有字符串
`torrent_id`。`ctx.settings` 是用户在此包的“本机配置”中填写的参数。

`max_pages` 为 unit 时表示没有指定页数上限；脚本仍受总体执行时间和 HTTP 次数限制。
站点如果一次返回全目录，应在 search 函数里按关键词过滤，不要将全目录当作搜索结果。

Cookie、Authorization 和 API Key 不直接暴露为脚本变量。HTTP 帮助函数从主程序
当次提供的站点凭据中注入；每次请求使用主程序最新同步的值。

### 3.4 资源结果

每条资源必须有稳定字符串 `id` 和非空 `title`；`size` 用字节，默认 0。
通常还需要 `page_url`、`description`、`seeders`、`leechers`、`publish_time`。

促销使用实际倍率：`downloadvolumefactor: 0.0` 表示免费，`uploadvolumefactor: 2.0`
表示双倍上传。未知倍率省略，不要凭枚举顺序猜测。H&R 使用 `hit_and_run`。
完整字段见 [插件接口文档](README.md#资源输出)。

种子 ID 应在链接换签或适配包升级后仍保持同一含义。不要用完整的临时下载链接作为 ID。
主程序将根据标题识别片目、年份、季集等，脚本无需重新实现这部分。

### 3.5 JSON API 示例

假设已经确认站点的 `/api/torrents` 返回以下结构：

```json
{"data":[{"id":42,"name":"Example Show S01E02 1080p","size":123456789,"seeders":5,"leechers":2}]}
```

对应 Rhai 示例（请根据真实接口修改）：

```rust
fn latest(ctx) {
    let response = http_get("/api/torrents");
    if response.status != 200 { throw "列表请求失败"; }
    let items = [];
    for row in response.json.data {
        items.push(#{
            id: row.id.to_string(),
            title: row.name,
            size: row.size,
            seeders: row.seeders,
            leechers: row.leechers,
            page_url: "/torrents/" + row.id.to_string()
        });
    }
    #{items: items}
}

fn search(ctx) {
    let keyword = ctx.input.keyword.to_lower();
    let items = [];
    for item in latest(ctx).items {
        if item.title.to_lower().contains(keyword) { items.push(item); }
    }
    #{items: items}
}

fn download(ctx) {
    let url = "/api/torrents/" + url_encode(ctx.input.torrent_id) + "/download";
    #{torrent_base64: torrent_get(url)}
}
```

上例下载路径是示意，需要真实验证。支持动态链接的站点，可以先用 `http_get` 或 `http`
请求详情/签发接口，从响应取得地址，再传给 `torrent_get`。

### 3.6 HTML 和 RSS

HTML 页面用 `html_select(html, css)`，返回数组，节点包含 `text`、`html`、`attrs`：

```rust
let response = http_get("/torrents.php");
let rows = html_select(response.body, "tr.torrent-row");
for row in rows {
    let links = html_select(row.html, "a.torrent-title");
    if links.len() > 0 {
        let title = links[0].text;
        let page_url = absolute_url(links[0].attrs.href);
        // 根据真实页面补齐 ID、大小和其他字段，然后构造 items。
    }
}
```

RSS 可用 `rss_items(http_get(ctx.site.rss_url).body)` 转换，但仍需把 guid 或详情页解析为
稳定 `id`。下载时重新读取 RSS 并按 ID 匹配 enclosure，可以刷新签名链接。
如果 RSS 只返回最新几十条，这种实现只能覆盖那些条目；无法据此宣称支持全站目录和旧种子。

### 3.7 HTTP 调用、认证和代理

```rust
let response = http(#{
    url: "/api/search",
    method: "POST",
    auth: "cookie",
    headers: #{"Content-Type": "application/json"},
    body: #{keyword: ctx.input.keyword, page: 1}
});
```

- 支持 GET、POST、HEAD；body 可为 JSON 对象或字符串。
- auth 默认为 all，也可指定 cookie、authorization、api_key、none。
- 脚本不能自行设置 Cookie、Authorization、X-API-Key、Host 请求头。
- 默认只向与当前站点同源的地址注入凭据。跨域凭据需要明确声明 credential_domains。
- 使用主程序站点的代理开关和主程序配置的代理地址。
- 文本编码默认读取响应 Content-Type；必要时在 http 选项中设置 encoding，例如 gbk。
- 普通请求返回 status/body/json；json 无法解析时为 unit，脚本应检查状态和业务错误。

### 3.8 用户资料和站内信

只有取得完整资料时才声明 profile。必须返回 user_level、ratio、uploaded、downloaded、
message_unread；上传下载用字节，累计值不能用当月统计替代。不要把缺失值写成零。

站内信返回 key/title/date/content/url。使用稳定消息 ID，并确认读取接口不会将消息标为已读。
预览界面只提供资源和用户资料读取，不会调用站内信操作。

## 4. 预览与排错

| 现象 | 检查项 |
|---|---|
| 未接管，仍走旧接口 | 插件启用、sites:adapt 权限、包启用、精确域名和 operations |
| HTTP 401/登录页 | 在主程序更新 Cookie 或同步 CookieCloud；核对接口所需认证方式 |
| 请求域名被拒绝 | 补充真实 API/CDN 域名到 network_domains；确认不是错误的重定向 |
| API 子域认证失败 | 核对是否需要 credential_domains，以及 auth 应为 api_key 还是 cookie |
| 脚本执行失败并显示位置 | 对照该行检查字段层级、空值和类型；异常正文为保护凭据会隐藏 |
| 抓到资源但下载失败 | 实现 download；核对旧种子下载、动态链接、额外域名和响应是否为真实种子 |
| 导入后无预览 | 新包默认停用；还需选择匹配站点并选择已声明的预览操作 |
| 新规则出错 | 回退上一份规则或停用，恢复主程序内置处理 |

资源 ID 和字段类型会在主程序接收时校验。适配更新会使搜索缓存失效，无需重启。
下载函数取得的文件还会交给主程序验证，不会把 HTML 登录页作为正常种子提交。

## 5. 导出与分享

点击“导出”得到 `.mediary-site.json`。接收者导入并填写自己的本机参数、启用即可。
包中不会自动带上本站 Cookie、RSS key 或本机参数；用户手工硬编码到脚本中的字符串
会随脚本导出，因此分享前仍需检查脚本本身。

分享时建议附上：站点域名、包版本、支持范围、站点改版日期、实际验证过的操作、
已知限制。仅做过 RSS 验证的包，应明确写“仅覆盖 RSS 返回的资源”。

## 6. 让 AI 协助编写的提示词

复制以下内容，并附上本文件、插件 README 和脱敏的真实接口材料：

```text
请为 Mediary 的“站点适配管理”插件编写一份可导入的 .mediary-site.json 适配包。

请先读取随附的 SITE_ADAPTER_GUIDE.md 和插件 README，严格使用适配包 v1 格式及
插件提供的 Rhai 函数。Rhai 不是 JavaScript，不要调用文件、shell、Node.js 或 Python。

目标站点：【填写域名】。
需求：优先完成 latest、search、download，使主程序能抓取正确的资源信息和取得真实种子。
订阅调度、片目识别、规则筛选、下载器管理、登录和 CookieCloud 同步仍由 Mediary 主程序负责。

请依据我附上的真实请求和响应实现，不要猜测 API、JSON 层级、促销枚举或下载地址。
资源 ID 必须稳定，size 使用字节。抓取必须配套实现 download，并确认目录中的旧种子也能下载。
动态下载地址应在 download 中按需获取。不要将临时签名 URL 作为资源 ID。

仅声明已实现的操作。profile 只有拿到完整累计流量、分享率、等级和未读数时才声明。
未知促销倍率省略，缺失流量不能填零，RSS 有数量限制时必须说明覆盖范围。

使用主程序注入的当前站点凭据。额外请求域名用 network_domains 声明；确实需要向 API 子域
携带本站凭据时才声明 credential_domains。个人参数仅声明 settings_fields，由导入者在本机填写。
不要把真实 Cookie、Token、RSS key、签名和用户身份信息写进包或报告。

请输出：
1. 完整可导入 JSON 文件，script 为有效 Rhai 源码字符串。
2. 支持的操作、所需本机参数和覆盖范围。
3. 在插件中执行资源预览的步骤，以及未能确认的接口或限制。
材料不足时请定向告诉我需要哪一个真实请求或响应，不要把静态推断描述成实际验证通过。
```

## 7. 开发者验证

以下命令在 Mediary 主仓库根目录执行。

```bash
cargo build --manifest-path plugins/site-adapter/Cargo.toml --locked
python3 tools/test-site-adapter.py
cargo test --manifest-path backend/Cargo.toml --locked services::site_adapter::tests
cargo test --manifest-path backend/Cargo.toml --locked isolated_site_adapter_pipeline -- --ignored --test-threads=1
```

这些验证使用临时目录、模拟 HTTP 站点和隔离数据库，不需要真实站点账号，不创建真实订阅或下载。
