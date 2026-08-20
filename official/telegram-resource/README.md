# TG搜索插件

适用于支持原生插件资源搜索源的 Mediary 版本。插件使用 Telegram 用户 API 搜索你已经加入的公开频道，并把消息中的 115 分享、magnet 和 ed2k 链接显示在 Mediary 主搜索页。

## 功能

- 使用 `api_id`、`api_hash`、手机号验证码及可选两步验证密码登录 Telegram 用户账号；
- 仅搜索插件配置中明确列出的公开频道；
- 从消息正文、文本链接和按钮链接中识别 115 分享、magnet 与 ed2k；
- 在 Mediary“资源搜索”中注册为 Telegram 搜索源；
- 115 分享提交给 FlowLink 转存整理；
- magnet 与 ed2k 提交到 Mediary 配置的 115 转存监控目录，并在 10 秒后触发一次 FlowLink `move all`；
- 推送前重新读取 Telegram 原消息并校验链接。

## 获取 Telegram 应用参数

1. 使用 Telegram 账号登录 <https://my.telegram.org/>。
2. 打开 **API development tools** 并创建应用。
3. 记录 `api_id` 和 `api_hash`。不要把 `api_hash` 或插件 Session 发送给其他人。

## 配置和登录

1. 在插件中心填写 API ID、API Hash、国际格式手机号和频道用户名。
2. 保存并启用插件，然后重新打开插件配置。
3. 点击“发送验证码”。Telegram 通常会把验证码发送到已登录的 Telegram 客户端。
4. 输入验证码并点击“完成登录”。如果提示需要两步验证，再填写密码并再次点击“完成登录”。
5. 不要点击保存验证码或两步验证密码；它们只用于当前登录动作。

频道每行填写一个，可使用 `@channel_name`、`channel_name` 或 `https://t.me/channel_name`。插件只接受公开用户名，不接受私有邀请链接。

## 推送要求

- 115 分享需要 Mediary 已启用并正确配置 FlowLink。
- magnet/ed2k 需要 Mediary 已配置 115 Web Cookie、转存目录 CID 和 FlowLink；资源会先进入转存监控目录，随后触发 FlowLink 整理。

## 安装

插件目录名必须为 `telegram-resource`，其中包含可执行文件 `plugin` 和 `plugin.json`：

```text
<Mediary 配置目录>/plugins/telegram-resource/
├── plugin
└── plugin.json
```

Docker 默认目录为 `/app/config/plugins/telegram-resource/`。安装或替换可执行文件后重启 Mediary。

## 安全说明

Telegram Session 保存在插件的 `data/telegram.session` 中，相当于账号登录凭据。请保护 Mediary 配置目录，不要共享该文件。退出登录会向 Telegram 注销当前 Session。
