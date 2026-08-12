# Seerr 订阅接入

该插件将 Seerr 的已批准请求转换为 Mediary 订阅。它不要求 Radarr 或
Sonarr，Seerr 可以继续负责媒体发现、用户权限和审批。

## Seerr Webhook 设置

- URL：`http://<Mediary 地址>:8118/api/plugins/seerr/external/webhook`
- Authorization Header：`Bearer <Mediary API Token>`
- 通知类型：Request Approved、Request Automatically Approved、Request Declined
- JSON Payload：

```json
{
  "notification_type": "{{notification_type}}",
  "event": "{{event}}",
  "subject": "{{subject}}",
  "message": "{{message}}",
  "image": "{{image}}",
  "{{media}}": {
    "media_type": "{{media_type}}",
    "tmdbId": "{{media_tmdbid}}",
    "status": "{{media_status}}",
    "status4k": "{{media_status4k}}"
  },
  "{{request}}": {
    "request_id": "{{request_id}}",
    "requestedBy_username": "{{requestedBy_username}}"
  },
  "{{extra}}": []
}
```

`{{extra}}` 必须保持为 JSON 对象键，Seerr 会在电视剧请求中将季度列表写入该字段。

## 行为

- `MEDIA_APPROVED` / `MEDIA_AUTO_APPROVED`：电影创建一个订阅，电视剧按季度拆分创建。
- `MEDIA_DECLINED`：启用拒绝同步时，仅删除该插件为对应 Seerr 请求创建且尚未完成的订阅。
- 其他事件会返回成功但不做处理。
- 插件使用 `request_id + TMDB ID + 季度` 保证幂等。

Seerr 直接删除请求时不会发送拒绝通知，因此该操作不会自动取消 Mediary
订阅。需要同步取消时，应在 Seerr 中使用 Decline。
