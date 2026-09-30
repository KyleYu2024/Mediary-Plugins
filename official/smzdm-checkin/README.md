# 什么值得买签到插件

使用什么值得买 App 的签到接口执行每日签到。优先使用从已登录的 iOS 11.1.95 App
签到中心 2.0 请求验证的签名协议；当服务端明确返回协议不兼容时，再尝试旧版 Android token 协议。
签到结果使用 `cpadd` 表示本次获得的积分；`cpoints` 是账户积分余额，不能当作本次奖励。
重复执行不会重复领取签到奖励。

## 配置

1. 在 Mediary 系统设置中配置 CookieCloud，并确保浏览器已登录什么值得买且同步的 `smzdm.com` Cookie 包含 `sess`。只同步统计 Cookie 无法签到。
2. 保持“优先使用 Mediary CookieCloud”开启。插件每次运行时读取最新 Cookie，不保存副本。
3. 可选填写一个账号的手工 Cookie，作为 CookieCloud 不可用时的兜底。
4. 设置 cron，默认 `23 7 * * *` 表示每天 07:23 执行，然后启用插件。

签到成功、已签到和失败通知默认使用
[什么值得买通知图](assets/notification.png)。

Cookie 属于账号凭据，不要将它发到 issue、日志或截图中。插件只获准读取 CookieCloud 中
`smzdm.com` 域名的 Cookie，不会接触 CookieCloud 密钥，也不会把 Cookie、token、用户 ID
或服务端响应原文写入历史记录和通知。

## 稳定性边界

- 只执行每日签到，不执行点赞、收藏、评论、抽奖、浏览任务或众测申请。
- 短暂网络故障和服务端 5xx 会进行一次有限重试。
- Cookie 失效、验证码或风控不会伪装成成功，也不会进行高频重试。
- 每个插件实例只支持一个账号；配置多行手工 Cookie 时会明确报错。
- 通知直接显示本次签到结果，不再显示多账号汇总。
- 什么值得买签到并非公开 API。服务端若同时停用两套 App 协议，仍需发布插件更新。
- 已在本机已登录的 iOS 11.1.95 App 观察到签到中心 2.0 仍向 `user-api.smzdm.com/checkin` 发送签名 POST，请求签名与插件算法一致，重复签到返回 `error_code=0`、`error_msg=已签到`。插件自身尚未用该账号的 Cookie 执行在线验证。
- 补签卡、勋章、折扣券等新奖励不属于本插件的自动领取范围。

实现所使用的接口形态和签名算法参考了仍在维护的开源项目
[`Cat-zaizai/ZaiZaiCat-Checkin`](https://github.com/Cat-zaizai/ZaiZaiCat-Checkin) 与
[`agluo/ql-script-hub`](https://github.com/agluo/ql-script-hub)，插件本身不下载或执行外部代码。
新版 iOS 参数与响应字段参考
[`enwaiax/smzdm-bot`](https://github.com/enwaiax/smzdm-bot)。
