# tgsrssr

**中文** · [English](#tgsrssr-english)

> **T**elegram's **R**SS **R**ust — RSS 订阅 Telegram 推送 Bot

把 RSS/Atom 源的新条目定时推送到 Telegram 频道/群组，并对**微博、X (Twitter)、Telegram 频道**提供原生抓取。富媒体转发、内容筛选、消息反查、断点续发、持久化重试一应俱全。

## 效果展示

以下频道由 tgsrssr 实际驱动，可直接查看各类源的推送效果：

| 示例频道 | 内容来源 | 内容方式 |
|---|---|---|
| [People's Daily](https://t.me/peoplesdailyzh) | 微博平台原生抓取 | 仅内容 |
| [Elon Musk](https://t.me/elonmuskxtot) | X (Twitter) 平台原生抓取 | 仅内容 |
| [Hack News](https://t.me/hackernewsyc) | 标准 RSS 源 | 标题+源超链 |

## 为什么选 tgsrssr

- **原生平台抓取**：微博（长文全文、多图补全、视频 HD 升级、投票转 Poll）、X（GraphQL API 原图原视频）、Telegram 频道
- **不丢消息**：发送失败自动降级纯文本；仍失败入 outbox 持久化重试队列（指数退避，72h 放弃），绝不静默丢失
- **富媒体管线**：图片/GIF/视频/文档自动识别分类发送，file_id 上传去重，超限自动转文档，媒体组成组发送
- **多用户 + 管理员分级**：白名单/开放注册准入 + 频道归属按邀请者判定；管理员统一配置平台 cookie，所有用户共享
- **单二进制部署**：Rust 编译产物 + SQLite，无外部数据库依赖，Docker 一键启动
- **轻量常驻**：稳态运行内存 ~20 MB（RSS，轻载实测）；媒体经内存上传时有短时峰值，compose 模板默认 1 GB 限制已预留余量

## 快速开始

### 前置条件

- Docker + Docker Compose
- 一个 Telegram Bot（通过 [TG@BotFather](https://t.me/BotFather) 创建，获取 Token）
- 你的 Telegram 用户 ID（可通过 [TG@userinfobot](https://t.me/userinfobot) 查询）

### 方式一：Docker 镜像（推荐，无需编译）

```bash
# 1. 复制配置模板
cp compose.example.yaml compose.yaml

# 2. 编辑 compose.yaml，填入必填项：
#    TELOXIDE_TOKEN: "你的 Bot Token"
#    ADMIN_USER_IDS: "你的用户ID"（管理员，有且只能一个）

# 3. 拉取镜像并启动
docker compose pull
docker compose up -d

# 4. 查看日志
docker logs -f tgsrssr
```

### 方式二：从源码构建（Docker）

```bash
# 编辑 compose.yaml：注释 image 行，取消注释 build: . 行
docker compose up -d --build   # 首次约 5-10 分钟
```

### 方式三：静态二进制（无需 Docker）

从 [Releases](../../releases) 下载 Linux 二进制 tar.gz（提供 x86_64 / aarch64 两种架构，musl 静态链接），解压后设置环境变量直接运行：

```bash
tar xzf tgsrssr-vX.Y.Z-<arch>.tar.gz
TELOXIDE_TOKEN=xxx ADMIN_USER_IDS=123 ./tgsrssr
```

每个压缩包都带一个同名 `.sha256` 校验文件，解压前可在同一目录验证下载完整性：

```bash
sha256sum -c tgsrssr-vX.Y.Z-<arch>.tar.gz.sha256
```

> 注意：二进制方式需自行安装 ffmpeg（视频缩略图/时长探测用）。

### 首次使用

1. 在 Telegram 中向你的 Bot 私聊发送 `/start`
2. **把 Bot 添加进你的频道或群组**（频道需确保有发消息权限）
3. 向 Bot 内发送 `/sub <频道ID> <RSS地址>` 开始订阅

> `/sub` 后面支持频道用户名（@xxx）或频道ID（-100xxxx）

### 可选：本地 Bot API 服务器（突破 50MB 上传限制）

默认使用 Telegram 官方 Bot API（上传限制 50MB）。如需推送大视频，可部署 [telegram-bot-api 本地服务器](https://github.com/aiogram/telegram-bot-api)，然后设置 `BOT_API_URL` 环境变量指向它。详见下方「可选增强」章节。

## 命令

| 命令 | 说明 |
|---|---|
| `/start` | 帮助 |
| `/set [频道]` | 频道管理面板 |
| `/list <频道>` | 查看频道订阅列表 |
| `/sub <频道> <url...>` | 添加订阅（可一次多个） |
| `/test <频道> <源地址> [条目序号]` | 立即推送指定源的某一条（0=最新） |
| `/find <TG消息链接>` | 按 TG 消息反查 RSS 条目；也可直接转发频道消息给 bot |
| `/cookies` | 平台登录 cookie 管理（仅管理员） |
| `/loget <日期>` | 获取指定日期的日志文件（仅管理员） |
| `/about` | 关于（版本信息） |

默认仅 `ADMIN_USER_IDS` 管理员与 `ALLOWED_USER_IDS` 白名单可用，其余用户静默忽略；设置 `OPEN_TO_ALL=true` 可对所有用户开放。
`/cookies`、`/loget` 仅管理员可用。管理员配置的平台 cookie 由所有用户共享；未配置时用户订阅相应平台源会收到提示。
`/list`、`/sub`、`/test` 在群组/频道内可省略频道参数，默认作用于当前会话。

## 支持的内容源

### 标准 RSS/Atom

任意 RSS 2.0 / Atom 源，支持 ETag / Last-Modified 条件请求省流量。

### 微博（原生抓取）

订阅格式：`https://weibo.com/u/<uid>`

- 需登录 cookie（管理员经 `/cookies` 配置）
- 长微博全文、多图补全（>9 张）、视频 HD 升级（1080p+）
- 转发以引用块呈现、表情转 emoji（约 150 个映射）、投票转 Telegram Poll 并自动置顶
- 头条文章封面图、直播封面图、「查看图片」卡片图片提取
- cookie 失效即时私聊通知管理员

### X / Twitter（原生抓取）

订阅格式：`https://x.com/<screen_name>`

- 需 `auth_token` cookie（管理员经 `/cookies` 配置）
- GraphQL API 抓取：长推文全文、原图（`?name=orig`）、最高码率视频
- t.co 短链展开、@mention/#hashtag/$cashtag 自动超链
- 转推/引用以 blockquote 呈现

### Telegram 频道（原生抓取）

订阅格式：`https://t.me/<channel>`

- 抓取公开频道（`t.me/s/<channel>`），无需登录态
- 保留超链接、回复引用以 blockquote 呈现
- 仅支持公开频道

## 频道管理面板（/set）

按钮式 UI，列表分页（每页 10 个）：

- 频道列表展示订阅源数量，源列表倒序（最新在前），暂停源带 ⏸ 标识
- 频道页：「频道设置」「退订管理」；批量退订含 OPML 备份
- 全部启用：一键恢复所有停用源
- 源级设置：暂停/恢复、改名、推送间隔/内容模式/来源显示/链接预览覆盖、内容筛选
- 频道级设置（二级页）：推送间隔（新频道默认 30 分钟）/ 内容模式 / 来源显示 / 链接预览
- 源级覆盖：每个源可单独覆盖频道设置（✏️ 标记），选择与频道相同自动清除覆盖

## 推送与渲染

- **内容模式**：`title_link` / `title_summary`（+140字摘要）/ `title_content`（全文）/ `title_only` / `content_only` / `media_only`
- **来源显示**：`source`（via 源标题）/ `author` / `none`
- 长文按可见长度切分（正文 4096 / 媒体说明 1024 UTF-16 码元），多段引用第一条
- 同频道发送集中排队（≥2 秒），触发限流按 Retry-After 退避
- HTML 解析失败自动降级纯文本重发
- 新条目按发布时间升序推送（旧→新）

## 媒体管线

- 图片/GIF/视频/文档自动识别 → sendPhoto / sendAnimation / sendVideo / sendMediaGroup / sendDocument
- 超限大图（长图/高宽比/尺寸）自动转文档保留原图；转文档媒体成组发送
- 流式落盘下载（边下载边写文件 + 增量 SHA-256），防大文件 OOM
- file_id 上传去重（内容哈希），同一媒体只上传一次
- 视频自动附带缩略图与时长（ffmpeg/ffprobe 生成）
- 单条最多 30 个媒体，超 10 个自动拆组
- 媒体发送失败自动降级纯文本，条目不丢失
- 可选图片中转反代（防盗链场景），失败自动直连兜底
- 视频超限（`MAX_VIDEO_PUSH_MB`）不推送，降级纯文本并通知

## 内容筛选

源级过滤（/set 源页「内容筛选」）：

- 黑名单（排除含关键词条目）/ 白名单（仅推送含关键词条目）
- 关键词逗号分隔，匹配标题，不区分大小写
- 典型场景：GitHub releases.atom 过滤 beta/rc 测试版

## 可靠性与自愈

- 抓取连续失败达阈值（默认 20 次）自动停源并通知；每小时自愈检查恢复
- **Outbox 持久化重试**：发送失败条目序列化入队，指数退避（1min→2h 封顶），72h 放弃并通知
- 分段级/op 级断点续发，只重发失败部分
- 平台级健康监控：连续 2 轮全部源失败→通知管理员「访问异常」；恢复→通知「恢复正常」
- 媒体下载 5xx 可重试（间隔 5s），4xx 不重试
- Bot 被移出频道时自动清空该频道全部数据

## 定时任务

| 任务 | 周期 | 说明 |
|---|---|---|
| 抓取推送 | 默认 5 分钟 | 按各源间隔抓取并推送新条目 |
| Outbox 重试 | 每 5 分钟 | 重发到期的失败条目 |
| 自愈 | 每小时 | 恢复过冷却期的自动停用源 |
| 清理 | 默认 60 分钟 | 过期媒体/日志/备份/缓存 |

## 配置参考（compose.yaml environment）

| 变量 | 说明 | 默认 |
|---|---|---|
| `TELOXIDE_TOKEN` | BotFather token（**必填**） | - |
| `ADMIN_USER_IDS` | 管理员，**必须配置且只允许一个**用户 ID；可用 /cookies、/loget，接收平台告警；为 0 个或多个时启动报错 | - |
| `ALLOWED_USER_IDS` | 用户白名单 ID，逗号分隔、可配多个（可选，默认为空；管理员隐式放行，仅管理员使用时留空即可） | 空 |
| `OPEN_TO_ALL` | true = 对所有用户开放；false = 仅管理员+白名单 | false |
| `BOT_API_URL` | 本地 Bot API 地址；留空 = 官方 API | 空 |
| `BOT_API_MEDIA_DIR` | bot-api 容器内媒体目录（file:// 零拷贝） | /var/lib/telegram-bot-api-media |
| `RSS_PROXY` | 抓取 SOCKS5 代理（如 `socks5h://127.0.0.1:1080`） | 空 |
| `RSS_NO_PROXY` | 不走代理的域名（逗号分隔） | 空 |
| `RSS_INSECURE_HOSTS` | 跳过 TLS 验证的域名 | 空 |
| `IMG_RELAY_ENABLED` | 防盗链图片中转开关 | true |
| `IMG_RELAY_BASE_URL` | 中转服务地址（空 = 不启用） | 空 |
| `IMG_RELAY_DOMAINS` | 走中转的域名（逗号分隔） | 空 |
| `MAX_VIDEO_PUSH_MB` | 视频推送上限（MB）；不用本地 bot-api 时受官方 API 50MB 上传限制约束，实际生效上限 49MB | 100 |
| `MAX_FETCH_ERRORS` | 连续失败停源阈值 | 20 |
| `SCHEDULER_INTERVAL_MINUTES` | 抓取调度周期（分钟） | 5 |
| `FETCH_CONCURRENCY` | 抓取并发上限 | 8 |
| `DOWNLOAD_CONCURRENCY` | 下载并发上限 | 4 |
| `MEDIA_OUTBOUND_TTL_HOURS` | 媒体文件保留（小时） | 80 |
| `MEDIA_MAX_SIZE_GB` | 媒体目录总配额（GB） | 10 |
| `LONG_IMAGE_ASPECT_RATIO` | 长图判定高宽比阈值 | 8.0 |
| `SENT_ENTRIES_RETENTION_DAYS` | 已发条目记录保留天数 | 180 |
| `CLEANUP_INTERVAL_MINUTES` | 清理任务周期（分钟） | 60 |
| `LOG_RETENTION_DAYS` | 日志保留天数 | 30 |
| `BACKUP_RETENTION_DAYS` | 数据库备份保留天数 | 7 |
| `FILE_ID_CACHE_RETENTION_DAYS` | file_id 缓存保留天数 | 30 |
| `SENT_MESSAGES_MAX_ROWS` | /find 反查登记表行数上限 | 5000 |
| `DB_PATH` | SQLite 路径（Docker 镜像内置 `/app/data/rss_bot.db`；静态二进制默认相对路径 `data/rss_bot.db`） | data/rss_bot.db |
| `MEDIA_DIR` | 媒体目录（Docker 镜像内置 `/app/media`；静态二进制默认相对路径 `media`） | media |
| `LOG_DIR` | 日志目录（Docker 镜像内置 `/app/logs`；静态二进制默认相对路径 `logs`） | logs |
| `RUST_LOG` | 日志级别（tracing EnvFilter） | info |

## 可选增强：本地 Bot API 服务器

默认直连 Telegram 官方 Bot API：媒体读入内存后上传，单文件上限 50MB（本 bot 保守取 **49MB**）。如需推送大视频/大文件，可自建本地 Bot API 服务器并启用 **file:// 零拷贝**——bot 把媒体写入与 bot-api 共享的目录，发送时以 `file://` 本地路径引用，bot-api 直接读盘，不经 HTTP 上传，单文件物理上限放宽到 **2GB**。

### 两种媒体传输模式

| 模式 | 触发条件 | 传输方式 | 单文件上限 |
|---|---|---|---|
| `memory`（官方 API） | `BOT_API_URL` 留空 | 读入内存上传 | 49MB |
| `file://`（本地零拷贝） | `BOT_API_URL` 已设 且 `BOT_API_MEDIA_DIR` 非空 | 本地路径直读 | 2GB |

实际是否推送某条视频，还叠加受业务闸门 `MAX_VIDEO_PUSH_MB`（默认 100）约束，与上表物理上限取更严者。由于 `BOT_API_MEDIA_DIR` 默认为非空值，**是否走零拷贝实际上只由 `BOT_API_URL` 是否为空决定**。

### 部署目录结构

file:// 的前提是两容器挂载同一宿主媒体目录。典型布局：

```text
deploy/
|-- tgsrssr/                  # bot 本体
|   |-- compose.yaml
|   |-- data/                 # SQLite（DB_PATH）
|   |-- logs/                 # 日志（LOG_DIR）
|   +-- media/                # 媒体落盘（MEDIA_DIR，容器内 /app/media）
+-- telegram-bot-api/         # 可选：本地 Bot API（官方 aiogram 镜像）
    |-- compose.yaml
    |-- data/                 # bot-api 运行时状态
    +-- media/                # 与 tgsrssr 共享的媒体目录
```

不用本地 bot-api 时，`tgsrssr/media/` 用独立本地目录即可，无需共享。

### 媒体目录配置（关键）

零拷贝不依赖绝对路径：bot 写入 `MEDIA_DIR`（`/app/media`），发送时把该前缀**翻译**成 `BOT_API_MEDIA_DIR` 前缀生成 `file://` 路径，bot-api 从同一宿主目录读文件。两侧配置：

```yaml
# telegram-bot-api/compose.yaml
services:
  telegram-bot-api:
    environment:
      TELEGRAM_API_ID: "<api_id>"          # my.telegram.org 申请，非 BotFather token
      TELEGRAM_API_HASH: "<api_hash>"
      TELEGRAM_LOCAL: "1"                   # 本地模式：允许 file:// 发送
    volumes:
      - ./data:/var/lib/telegram-bot-api
      - ./media:/var/lib/telegram-bot-api-media   # bot-api 容器内的媒体路径

# tgsrssr/compose.yaml
services:
  tgsrssr:
    environment:
      BOT_API_URL: "http://127.0.0.1:8081"                 # 指向本地 bot-api
      BOT_API_MEDIA_DIR: "/var/lib/telegram-bot-api-media" # = bot-api 侧媒体挂载路径
    volumes:
      - ./data:/app/data
      - ../telegram-bot-api/media:/app/media                      # 换成与 bot-api 同一宿主目录
      - ../telegram-bot-api/media:/var/lib/telegram-bot-api-media
      - ./logs:/app/logs
```

要点：`BOT_API_MEDIA_DIR` 默认即 `/var/lib/telegram-bot-api-media`（官方镜像约定），仅当 bot-api 用了不同容器内路径才需覆盖；两个容器**必须挂同一宿主目录**，否则 `file://` 发送失败。`TELEGRAM_LOCAL=1` 与 `BOT_API_URL` 指向本地，是启用零拷贝的两把开关。

## 通知

频道归属由邀请者决定（谁拉 bot 进频道谁拥有），以下通知私聊发送给频道属主：

- 源自动停用（达失败阈值）
- 媒体下载/发送失败
- 视频超限跳过
- 推送失败入队 / 72h 放弃

以下通知发送给管理员（`ADMIN_USER_IDS`）：

- 平台 cookie 失效
- 平台访问异常与恢复

## 数据表

| 表 | 用途 |
|---|---|
| `chats` | 频道/群组登记 |
| `subscriptions` | 订阅（含失败计数、协商头、改名前原标题） |
| `chat_configs` / `feed_configs` | 频道级 / 源级设置 |
| `sent_entries` | 已推送条目去重 |
| `sent_messages` | TG 消息 → RSS 条目映射（供 /find） |
| `file_id_cache` | 内容哈希 → file_id（上传去重） |
| `settings` | 键值设置（cookie 等运行时配置） |
| `outbox` | 发送失败持久化重试队列 |

## 开发者：从源码构建

```bash
# 需要 Rust 1.96+，sqlx 离线模式（无需运行中的数据库）
SQLX_OFFLINE=true cargo build --release
# 产物在 target/release/tgsrssr
```

CI（GitHub Actions）在打 tag 时自动构建多平台 Docker 镜像（linux/amd64 + linux/arm64）
并上传静态二进制到 Releases（每个包附带同名 `.sha256` 校验文件），见 `.github/workflows/release.yml`。

## 跨平台

代码不依赖平台特定 API，路径全部使用 `PathBuf`，可在 Linux / macOS / Windows(WSL) 编译运行。Docker 部署推荐 Linux 环境（`network_mode: host` 仅 Linux 原生支持；macOS/Windows Docker Desktop 需改用 bridge 网络并调整代理配置）。

## 许可

基于 MIT 许可发布，见 [LICENSE](LICENSE)。

---

# tgsrssr (English)

[中文](#tgsrssr) · **English**

> **T**elegram's **R**SS **R**ust — an RSS subscription Telegram push bot

Periodically pushes new entries from RSS/Atom feeds to Telegram channels/groups, with **native fetching for Weibo, X (Twitter), and Telegram channels**. Rich media forwarding, content filtering, message reverse lookup, resumable sending, and persistent retry are all built in.

## Showcase

The following channels are powered by tgsrssr — check them out to see the push quality for each source type:

| Demo Channel | Content Source | Content Mode |
|---|---|---|
| [People's Daily](https://t.me/peoplesdailyzh) | Weibo native fetch | Content only |
| [Elon Musk](https://t.me/elonmuskxtot) | X (Twitter) native fetch | Content only |
| [Hack News](https://t.me/hackernewsyc) | Standard RSS feed | Title + source link |

## Why tgsrssr

- **Native platform fetching**: Weibo (full long-form text, multi-image completion, HD video upgrade, polls → TG Poll), X (GraphQL API with original images/videos), Telegram channels
- **No message loss**: send failures automatically fall back to plain text; if that still fails, entries go into a persistent outbox retry queue (exponential backoff, 72h abandonment) — never silently lost
- **Rich media pipeline**: images/GIFs/videos/documents auto-detected and sent via the appropriate API, file_id upload dedup, oversized media auto-converted to documents, media grouped into albums
- **Multi-user + admin tiers**: whitelist/open registration access + channel ownership by inviter; admins configure platform cookies once, shared by all users
- **Single binary deployment**: Rust binary + SQLite, no external database, one-command Docker startup
- **Lightweight footprint**: ~20 MB RSS in steady state (measured under light load); in-memory media uploads cause short-lived peaks — the compose template's default 1 GB limit leaves ample headroom

## Quick Start

### Prerequisites

- Docker + Docker Compose
- A Telegram Bot (create via [TG@BotFather](https://t.me/BotFather) to get the token)
- Your Telegram user ID (query via [TG@userinfobot](https://t.me/userinfobot))

### Option 1: Docker image (recommended, no compilation)

```bash
# 1. Copy the config template
cp compose.example.yaml compose.yaml

# 2. Edit compose.yaml, fill in the required fields:
#    TELOXIDE_TOKEN: "your Bot token"
#    ADMIN_USER_IDS: "your user ID" (admin: required, exactly one ID)

# 3. Pull the image and start
docker compose pull
docker compose up -d

# 4. Check logs
docker logs -f tgsrssr
```

### Option 2: Build from source (Docker)

```bash
# Edit compose.yaml: comment out the image line, uncomment build: .
docker compose up -d --build   # ~5-10 minutes on first run
```

### Option 3: Static binary (no Docker)

Download a Linux tar.gz from [Releases](../../releases) (x86_64 / aarch64, statically linked against musl), extract, set env vars, and run:

```bash
tar xzf tgsrssr-vX.Y.Z-<arch>.tar.gz
TELOXIDE_TOKEN=xxx ADMIN_USER_IDS=123 ./tgsrssr
```

Each archive ships with a same-named `.sha256` checksum file; verify a download in the same folder before extracting:

```bash
sha256sum -c tgsrssr-vX.Y.Z-<arch>.tar.gz.sha256
```

> Note: the binary approach requires ffmpeg installed separately (for video thumbnails/duration probing).

### First Use

1. Send `/start` to your Bot in a private Telegram chat
2. **Add the Bot to your channel or group** (in channels, ensure it has permission to send messages)
3. Send `/sub <channel ID> <RSS URL>` to the Bot in the private chat to start subscribing

> After `/sub` you can use a channel username (@xxx) or a channel ID (-100xxxx)

### Optional: Local Bot API server (bypass the 50MB upload limit)

By default the official Telegram Bot API is used (50MB upload limit). To push large videos, deploy a [telegram-bot-api local server](https://github.com/aiogram/telegram-bot-api) and point the `BOT_API_URL` env var at it. See the "Optional Enhancement" section below.

## Commands

| Command | Description |
|---|---|
| `/start` | Help |
| `/set [channel]` | Channel management panel |
| `/list <channel>` | View channel subscription list |
| `/sub <channel> <url...>` | Add subscriptions (multiple at once) |
| `/test <channel> <feed URL> [entry index]` | Immediately push a specific entry from a feed (0 = latest) |
| `/find <TG message link>` | Reverse-lookup an RSS entry from a TG message; or forward a channel message to the bot |
| `/cookies` | Platform login cookie management (admin only) |
| `/loget <date>` | Get the log file for a specific date (admin only) |
| `/about` | About (version info) |

By default only `ADMIN_USER_IDS` admins and `ALLOWED_USER_IDS` whitelist members can use the bot; other users are silently ignored. Set `OPEN_TO_ALL=true` to open to all users.
`/cookies` and `/loget` are admin-only. Platform cookies configured by admins are shared by all users; subscribing to a platform source without a configured cookie shows a notice.
`/list`, `/sub`, `/test` can omit the channel argument inside a group/channel — the current chat is used.

## Supported Sources

### Standard RSS/Atom

Any RSS 2.0 / Atom feed, with ETag / Last-Modified conditional requests to save bandwidth.

### Weibo (native fetch)

Subscription format: `https://weibo.com/u/<uid>`

- Requires a login cookie (admin configures via `/cookies`)
- Full long-form text, multi-image completion (>9 images), HD video upgrade (1080p+)
- Retweets rendered as blockquotes, emoji markers converted (~150 mappings), polls converted to Telegram Polls and auto-pinned
- Article cover images, live-stream covers, "view image" card image extraction
- Cookie expiry instantly notified to admins via private message

### X / Twitter (native fetch)

Subscription format: `https://x.com/<screen_name>`

- Requires an `auth_token` cookie (admin configures via `/cookies`)
- GraphQL API fetching: full long tweets, original images (`?name=orig`), highest-bitrate video
- t.co short link expansion, auto-hyperlinking of @mentions/#hashtags/$cashtags
- Retweets/quotes rendered as blockquotes

### Telegram channels (native fetch)

Subscription format: `https://t.me/<channel>`

- Fetches public channels (`t.me/s/<channel>`), no login required
- Preserves hyperlinks; reply quotes rendered as blockquotes
- Public channels only

## Channel Management Panel (/set)

Button-based UI with paginated lists (10 per page):

- Channel list shows subscription counts; source list in reverse order (newest first); paused sources marked with ⏸
- Channel page: "Channel settings" and "Unsubscribe management"; batch unsubscribe includes OPML backup
- Enable all: one-click resume of all paused sources
- Per-source settings: pause/resume, rename, interval/content mode/source display/link preview overrides, content filter
- Channel-level settings (secondary page): push interval (default 30 min for new channels) / content mode / source display / link preview
- Per-source overrides: each source can override channel settings (✏️ marker); choosing the same value as the channel auto-clears the override

## Push & Rendering

- **Content modes**: `title_link` / `title_summary` (+140-char summary) / `title_content` (full text) / `title_only` / `content_only` / `media_only`
- **Source display**: `source` (via feed title) / `author` / `none`
- Long text split by visible length (body 4096 / media caption 1024 UTF-16 code units); multiple parts reply to the first
- Per-channel send queue (≥2s spacing); rate-limit triggers Retry-After backoff
- HTML parse failure auto-falls back to plain text resend
- New entries pushed in ascending publish-time order (old → new)

## Media Pipeline

- Images/GIFs/videos/documents auto-detected → sendPhoto / sendAnimation / sendVideo / sendMediaGroup / sendDocument
- Oversized images (long/aspect-ratio/size) auto-converted to documents preserving originals; document media sent as groups
- Streaming download to disk (write while downloading + incremental SHA-256), preventing large-file OOM
- file_id upload dedup (content hash) — same media uploaded only once
- Videos automatically include thumbnails and duration (generated by ffmpeg/ffprobe)
- Max 30 media per entry; auto-split into groups beyond 10
- Media send failure auto-falls back to plain text — entries never lost
- Optional image relay proxy (for hotlink-protected scenarios), auto-falls back to direct connection
- Videos exceeding `MAX_VIDEO_PUSH_MB` are not pushed; falls back to plain text with notification

## Content Filtering

Per-source filtering (/set source page → "Content filter"):

- Blacklist (exclude entries containing keywords) / Whitelist (only push entries containing keywords)
- Comma-separated keywords, matched against titles, case-insensitive
- Typical use case: filtering beta/rc pre-releases from GitHub releases.atom

## Reliability & Self-Healing

- Consecutive fetch failures reaching threshold (default 20) auto-disable the source with notification; hourly self-heal check resumes it
- **Outbox persistent retry**: failed entries serialized into a queue, exponential backoff (1min → 2h cap), abandoned after 72h with notification
- Segment-level/op-level resumable sending — only failed parts are resent
- Platform-level health monitoring: 2 consecutive rounds of all-source failure → "access error" notification to admins; recovery → "recovered" notification
- Media download 5xx retryable (5s interval), 4xx not retried
- Bot removed from a channel → all data for that channel auto-purged

## Scheduled Jobs

| Job | Interval | Description |
|---|---|---|
| Fetch & push | Default 5 min | Fetch each source at its interval and push new entries |
| Outbox retry | Every 5 min | Resend due failed entries |
| Self-heal | Hourly | Resume auto-disabled sources past cooldown |
| Cleanup | Default 60 min | Expired media/logs/backups/caches |

## Configuration Reference (compose.yaml environment)

| Variable | Description | Default |
|---|---|---|
| `TELOXIDE_TOKEN` | BotFather token (**required**) | - |
| `ADMIN_USER_IDS` | Admin, **required, exactly one** user ID; can use /cookies, /loget, receives platform alerts; startup fails with an error if 0 or multiple IDs are given | - |
| `ALLOWED_USER_IDS` | Whitelisted user IDs, comma-separated, multiple allowed (optional; empty by default — the admin is implicitly allowed, leave empty for admin-only use) | empty |
| `OPEN_TO_ALL` | true = open to all users; false = admins + whitelist only | false |
| `BOT_API_URL` | Local Bot API address; empty = official API | empty |
| `BOT_API_MEDIA_DIR` | Media dir path inside bot-api container (file:// zero-copy) | /var/lib/telegram-bot-api-media |
| `RSS_PROXY` | Fetch SOCKS5 proxy (e.g. `socks5h://127.0.0.1:1080`) | empty |
| `RSS_NO_PROXY` | Domains bypassing the proxy (comma-separated) | empty |
| `RSS_INSECURE_HOSTS` | Domains skipping TLS verification | empty |
| `IMG_RELAY_ENABLED` | Hotlink-protection image relay toggle | true |
| `IMG_RELAY_BASE_URL` | Relay service address (empty = disabled) | empty |
| `IMG_RELAY_DOMAINS` | Domains routed through the relay (comma-separated) | empty |
| `MAX_VIDEO_PUSH_MB` | Video push size limit (MB); without a local bot-api the official API's 50MB upload limit applies, so the effective cap is 49MB | 100 |
| `MAX_FETCH_ERRORS` | Consecutive failure threshold to disable a source | 20 |
| `SCHEDULER_INTERVAL_MINUTES` | Fetch scheduling interval (minutes) | 5 |
| `FETCH_CONCURRENCY` | Max concurrent fetches | 8 |
| `DOWNLOAD_CONCURRENCY` | Max concurrent downloads | 4 |
| `MEDIA_OUTBOUND_TTL_HOURS` | Media file retention (hours) | 80 |
| `MEDIA_MAX_SIZE_GB` | Total media directory quota (GB) | 10 |
| `LONG_IMAGE_ASPECT_RATIO` | Aspect ratio threshold for long-image detection | 8.0 |
| `SENT_ENTRIES_RETENTION_DAYS` | Sent entry record retention (days) | 180 |
| `CLEANUP_INTERVAL_MINUTES` | Cleanup job interval (minutes) | 60 |
| `LOG_RETENTION_DAYS` | Log retention (days) | 30 |
| `BACKUP_RETENTION_DAYS` | Database backup retention (days) | 7 |
| `FILE_ID_CACHE_RETENTION_DAYS` | file_id cache retention (days) | 30 |
| `SENT_MESSAGES_MAX_ROWS` | /find reverse-lookup registry row limit | 5000 |
| `DB_PATH` | SQLite path (Docker image sets `/app/data/rss_bot.db`; static binary defaults to relative `data/rss_bot.db`) | data/rss_bot.db |
| `MEDIA_DIR` | Media directory (Docker image sets `/app/media`; static binary defaults to relative `media`) | media |
| `LOG_DIR` | Log directory (Docker image sets `/app/logs`; static binary defaults to relative `logs`) | logs |
| `RUST_LOG` | Log level (tracing EnvFilter) | info |

## Optional Enhancement: Local Bot API Server

By default the bot connects directly to the official Telegram Bot API: media is read into memory and uploaded, with a 50MB per-file limit (this bot conservatively caps at **49MB**). To push large videos/files, deploy a local Bot API server and enable **file:// zero-copy** — the bot writes media into a directory shared with bot-api, references it via a `file://` local path when sending, and bot-api reads the file directly from disk with no HTTP upload, raising the physical per-file cap to **2GB**.

### Two Media Transport Modes

| Mode | Trigger | Transport | Per-file cap |
|---|---|---|---|
| `memory` (official API) | `BOT_API_URL` empty | read into memory, upload | 49MB |
| `file://` (local zero-copy) | `BOT_API_URL` set AND `BOT_API_MEDIA_DIR` non-empty | direct local path read | 2GB |

Whether a given video is actually pushed is further gated by the business limit `MAX_VIDEO_PUSH_MB` (default 100) — the stricter of that and the physical caps above applies. Since `BOT_API_MEDIA_DIR` is non-empty by default, **zero-copy is effectively decided solely by whether `BOT_API_URL` is set**.

### Deployment Directory Layout

file:// requires both containers to mount the same host media directory. Typical layout:

```text
deploy/
|-- tgsrssr/                  # the bot itself
|   |-- compose.yaml
|   |-- data/                 # SQLite (DB_PATH)
|   |-- logs/                 # logs (LOG_DIR)
|   +-- media/                # media on disk (MEDIA_DIR, /app/media inside the container)
+-- telegram-bot-api/         # optional: local Bot API (official aiogram image)
    |-- compose.yaml
    |-- data/                 # bot-api runtime state
    +-- media/                # media directory shared with tgsrssr
```

Without a local bot-api, `tgsrssr/media/` can be a standalone local directory — no sharing needed.

### Media Directory Configuration (key)

Zero-copy relies on no absolute paths: the bot writes into `MEDIA_DIR` (`/app/media`); when sending, that prefix is **translated** into a `file://` path prefixed with `BOT_API_MEDIA_DIR`, and bot-api reads the same host directory. Both sides:

```yaml
# telegram-bot-api/compose.yaml
services:
  telegram-bot-api:
    environment:
      TELEGRAM_API_ID: "<api_id>"          # from my.telegram.org, NOT the BotFather token
      TELEGRAM_API_HASH: "<api_hash>"
      TELEGRAM_LOCAL: "1"                   # local mode: allows file:// sending
    volumes:
      - ./data:/var/lib/telegram-bot-api
      - ./media:/var/lib/telegram-bot-api-media   # media path inside the bot-api container

# tgsrssr/compose.yaml
services:
  tgsrssr:
    environment:
      BOT_API_URL: "http://127.0.0.1:8081"                 # point at the local bot-api
      BOT_API_MEDIA_DIR: "/var/lib/telegram-bot-api-media" # = bot-api's media mount path
    volumes:
      - ./data:/app/data
      - ../telegram-bot-api/media:/app/media                      # replace with the same host dir as bot-api
      - ../telegram-bot-api/media:/var/lib/telegram-bot-api-media
      - ./logs:/app/logs
```

Key points: `BOT_API_MEDIA_DIR` defaults to `/var/lib/telegram-bot-api-media` (the official image's convention) — override it only if your bot-api uses a different in-container path; both containers **must mount the same host directory**, otherwise file:// sending fails. `TELEGRAM_LOCAL=1` plus `BOT_API_URL` pointing at the local server are the two switches that enable zero-copy.

## Notifications

Channel ownership is determined by the inviter (whoever adds the bot to a channel owns it). The following notifications are sent to the channel owner via private message:

- Source auto-disabled (failure threshold reached)
- Media download/send failure
- Oversized video skipped
- Push failure queued / 72h abandonment

The following notifications are sent to admins (`ADMIN_USER_IDS`):

- Platform cookie expiry
- Platform access errors and recovery

## Database Tables

| Table | Purpose |
|---|---|
| `chats` | Channel/group registry |
| `subscriptions` | Subscriptions (incl. failure counts, negotiation headers, original title before rename) |
| `chat_configs` / `feed_configs` | Channel-level / source-level settings |
| `sent_entries` | Pushed entry dedup |
| `sent_messages` | TG message → RSS entry mapping (for /find) |
| `file_id_cache` | Content hash → file_id (upload dedup) |
| `settings` | Key-value settings (cookies and other runtime config) |
| `outbox` | Persistent retry queue for failed sends |

## Developers: Build from Source

```bash
# Requires Rust 1.96+, sqlx offline mode (no running database needed)
SQLX_OFFLINE=true cargo build --release
# Output at target/release/tgsrssr
```

CI (GitHub Actions) automatically builds multi-platform Docker images (linux/amd64 + linux/arm64)
on tag push and uploads static binaries to Releases (each archive ships with a same-named `.sha256`
checksum file) — see `.github/workflows/release.yml`.

## Cross-Platform

The code has no platform-specific API dependencies; all paths use `PathBuf`. Compiles and runs on Linux / macOS / Windows(WSL). Docker deployment is recommended on Linux (`network_mode: host` is only natively supported on Linux; macOS/Windows Docker Desktop requires bridge networking with adjusted proxy configuration).

## License

Released under the MIT license. See [LICENSE](LICENSE).
