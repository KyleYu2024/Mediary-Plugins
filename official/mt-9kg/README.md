# mt-9kg

`mt-9kg` 启用后会在 Mediary 的“资源搜索 -> 搜索源选择”中注册同名搜索源。

- 只搜索 M-Team 成人分区，不会改变主程序默认的 M-Team 普通区搜索。
- 使用 Mediary 中已启用的 M-Team 站点配置，插件不会获取 API Key、Authorization 或 Cookie。
- 搜索结果在原生资源列表中展示，直接提交到 Mediary 当前配置的默认下载器。
- 保存路径和分类都是下载器参数；插件只添加原始下载任务，不做 TMDB 匹配、订阅或媒体整理。
- 停用或卸载插件后，`mt-9kg` 搜索源会自动消失。

使用前请先在 Mediary 站点设置中启用 M-Team，并配置有效的 API Key 和 Authorization。
