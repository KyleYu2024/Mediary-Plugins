# PT 流量管家

PT 流量管家是完全使用 Rust 编写的 Mediary 插件，用于按站点调度 PT 选种、限制刷流资源
占用、托管下载器任务并按规则清理种子。插件以 GPL-3.0-only 独立发布，要求 Mediary
0.8.8 或更高版本。

## 功能

- 为每个站点建立独立任务，可选 qBittorrent 或 Transmission。
- 支持列表页和站点 RSS 两种选种来源，以及五段 CRON 或固定分钟周期。
- 支持免费、2X 免费、H&R、正则、大小、做种人数、发布时间和订阅标题排除。
- 支持全局与任务级保种体积、下载数量、上传和下载带宽保护。
- 支持做种时间、H&R 做种时间、分享率、上传量、下载超时、无活动时间和平均上传速度删种。
- 支持促销结束清理未完成任务，以及按做种时间优先级执行动态删种。
- 删除前请求 Tracker 重新汇报；动态删种与删除文件默认关闭或需要用户明确配置。
- 通过标签隔离托管任务，并阻止跨站点重复添加同名种子。

## 安装与配置

在 Mediary 的插件市场安装 `PT 流量管家`，审核并授予所需权限，然后在已安装插件中启用。
打开插件工作台后，可以从已配置站点创建任务。RSS 模式需要先在 Mediary 站点编辑页填写该
站点的 RSS 地址。

建议先以较小的“每轮最多添加”值运行，并保持动态删种关闭。确认选种、保存目录、分类、
标签和下载器均符合预期后，再逐项启用删种条件。

## 权限

| Scope | 用途 |
| --- | --- |
| `torrents:read` | 从选定站点读取候选种子 |
| `downloads:create` | 向指定下载器添加任务 |
| `downloader:read` | 读取下载器能力和任务状态 |
| `downloads:write` | 限速、标签、暂停、重新汇报和删种 |
| `notifications:send` | 通过 Mediary 已配置渠道发送运行通知 |
| `subscriptions:read` | 排除现有订阅标题 |

插件不会读取站点 Cookie、下载器密码或 Mediary 配置文件。它只通过宿主签发的短期 Bearer
Token 调用已授权的本机插件 API。

## 数据与隐私

任务、托管记录和运行诊断保存在插件自己的 `data/state.json`。写入使用进程锁和原子替换，
插件升级时宿主会保留整个 `data/` 目录。插件不包含遥测，也不会把数据发送到 Mediary
配置之外的服务。

## 构建

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

发布包根目录只包含 `plugin.json` 和重命名为 `plugin` 的可执行文件。

## 许可证

本插件使用 [GNU General Public License v3.0](LICENSE)。Mediary 宿主只提供通用插件 API
和声明式交互视图，不包含本插件的 PT 业务实现。
