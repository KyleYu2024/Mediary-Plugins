# 猫眼榜单订阅

定时读取猫眼榜单，经 Mediary 匹配 TMDB 后创建订阅。

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
