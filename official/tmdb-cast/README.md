# TMDB 演员资料

适用于支持 `media-cast:write` 权限的 Mediary 版本。插件把电影与剧集的 TMDB 演员、角色和头像同步到内置媒体库；点击媒体详情中的演员，可查看其资料及本库关联作品。

1. 安装插件，授予 `catalog:read` 和 `media-cast:write` 权限。
2. 在 Mediary 主程序设置中配置 TMDB API Key，并启用插件。插件通过主程序接口获取演员资料，不读取或保存 Key；主程序原有的 TMDB 代理配置也会生效。
3. 手动运行「同步演员数据」处理现有库存。每次最多处理数量默认是 150；在设置里改为 `0` 可一次处理全部待同步作品。全量同步可能运行较久。之后新资源完成媒体库扫描时会自动调用插件；主程序的「补全缺失元数据」完成后也会触发。计划任务每天 04:10 补漏。

仅处理已匹配 TMDB ID 的电影和剧集。TMDB 暂时失败的项目保留待下次重试；媒体的 TMDB ID 变化后会重新同步。演员作品只展示当前内置媒体库中存在的电影与剧集。

Web 端和 Emby 兼容接口的演员头像均经由 Mediary 图片接口按需加载，缓存到 `/app/config/cache/actor-portraits/`。演员头像缓存不设容量上限；其他远程海报和背景图仍使用有上限的 `/app/config/cache/media-artwork/`。数据库只保存 TMDB 图片路径。

使用内置 Emby 兼容接口的第三方播放器可在电影、剧集详情的 `People` 字段读取演员，也可访问 `/emby/Persons`、`/emby/Persons/{name}`、`/emby/Persons/{id}/Credits` 和演员头像接口；访问范围遵循媒体账号的媒体库权限。
