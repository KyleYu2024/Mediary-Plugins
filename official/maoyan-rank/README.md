# 猫眼榜单订阅

定时读取猫眼榜单，经 Mediary 匹配 TMDB 后创建订阅。

## 0.2.4 更新

- 电视剧、网剧、综艺改用官网当前 `/i/api/encrypt/dashboard/webHeatData`；全网查询不传 `platformType`，独立平台保留腾讯 3、爱奇艺 2、优酷 1、芒果 7 的筛选。
- 按北京时间生成 `YYYYMMDD` 日期，不依赖主机或容器时区。
- 网络电影改用 `/i/api/encrypt/dashboard/webHeatNetData` 的 `dataList.list` 和 `movieName`；当日榜单为空且官网提供更早的最新可用日期时，仅重试一次。
- 每轮读取一次官网页面，复用页面提供的匿名 Cookie 和 `uid` / `uuid` / `channelId` 请求头。这些会话信息仅保存在本轮内存中，不写入配置、报告或日志。
- 保持轻量 HTTP 实现，不引入 Chromium、浏览器驱动或浏览器下载。普通 HTTP 不执行官网校验脚本，新接口仍可能返回 403；此时仅电视剧全网榜回退到公开综合榜，报告包含新接口失败原因和实际回退来源。独立平台、网剧和综艺失败时明确记录，不用综合榜替代。

## 0.2.3 更新

- 单个榜单请求失败时，继续处理其他成功取得的榜单，并将失败原因写入 `data/last-run.json`。
- 电视剧全网热度接口失败时，改用猫眼公开移动页面 `/web-heat` 中的电视剧＋网剧全网综合榜。订阅记录的来源明确标为“电视剧＋网剧全网综合榜”，运行报告的 `warnings` 同时说明范围变化。
- 全部请求失败时保存失败报告并返回失败；缺少列表的接口响应也视为失败，避免错误显示为获取零条。
- 保留既有订阅查重、季度和集数处理。

备用页面只提供全网综合榜，不能准确区分电视剧、网剧、综艺、网络电影或各独立平台。独立榜单仍使用各自接口；接口被猫眼拒绝时会记录失败，不会将综合榜冒充这些榜单。2026-10-09 验证时旧热度和网络电影接口返回 HTTP 403，电影票房接口及全网综合榜页面可用。

## 验证

```sh
cargo test --manifest-path official/Cargo.toml --locked --bin maoyan-rank-plugin
# 包含真实网络取数，仅访问猫眼，不调用 Mediary 或创建订阅：
cargo test --manifest-path official/Cargo.toml --locked --bin maoyan-rank-plugin -- --include-ignored
```

插件版本在 `plugin.json` 中维护。官方 `plugins-v*` tag 触发 GitHub Actions 构建发布包、创建 Release 并自动更新商店索引。
