# HDHive 插件

适用于 Mediary 1.8.0 及以上版本。插件直接连接 HDHive 作者提供的服务，不需要自行部署中转服务器。

## 功能

- 浏览器一键登录 HDHive；
- 普通签到、赌狗签到和定时签到，完成后通过 Mediary 通知系统发送结果；
- 按名称或 TMDB ID 搜索电影、剧集与影巢资源，并显示发布者与发布时间；
- 插件启用后自动成为 Mediary“资源搜索”中的 HDHive 搜索源；
- 115 分享链接提交到 Mediary 设置的转存目录；
- ed2k 链接提交到同一个转存目录，并在 10 秒后触发一次 FlowLink `move all`。

## 安装

1. 根据 Mediary 所在设备选择对应平台压缩包。
2. 创建 `<Mediary 配置目录>/plugins/hdhive/`。
3. 将压缩包内的 `plugin` 和 `plugin.json` 解压到该目录。
4. 确保 `plugin` 具有可执行权限：`chmod 755 plugin`。
5. 重启 Mediary，在“插件中心”打开 HDHive 配置并登录。

Docker 默认安装目录为 `/app/config/plugins/hdhive/`。升级时只替换 `plugin` 和 `plugin.json`，不要删除 Mediary 生成的 `config.json` 与 `data/`。

## 校验

下载后使用同目录的 `SHA256SUMS` 核对压缩包完整性：

```bash
sha256sum -c SHA256SUMS
```

macOS 可使用：

```bash
shasum -a 256 -c SHA256SUMS
```
