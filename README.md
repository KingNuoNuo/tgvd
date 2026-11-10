# TGVD · 电报视频下载器

> 一个 Windows 单文件 GUI，用 **MTProto 账号**直连 Telegram，批量下载频道 / 群里的视频：频道内关键字搜索 → 勾选 → 排队下载，多任务、多段并发、断点续传，全程不依赖任何第三方解析站。

[![build](https://github.com/KingNuoNuo/tgvd/actions/workflows/build.yml/badge.svg)](https://github.com/KingNuoNuo/tgvd/actions/workflows/build.yml)
[![release](https://img.shields.io/github/v/release/KingNuoNuo/tgvd?display_name=tag&sort=semver)](https://github.com/KingNuoNuo/tgvd/releases/latest)

![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D6)
![rust](https://img.shields.io/badge/Rust-edition%202021-CE422B)
![ui](https://img.shields.io/badge/GUI-egui%2Feframe-1F6FEB)
![mtproto](https://img.shields.io/badge/protocol-MTProto-2CA5E0)
![license](https://img.shields.io/badge/license-MIT-green)

---

## 为什么不用网页解析

`t.me/s/xxx` 这类匿名预览页只对**允许网页预览**的公开频道有效。一旦频道开启了「限制保存内容」或对地区 / 账号做了过滤，页面只会返回一张封面，任何下载器都拿不到直链。

TGVD 走的是**真·账号通道**：和官方客户端一样用 MTProto 登录，能看到的视频就能下，不受网页预览限制。

## 功能

| | |
|---|---|
| **频道内搜索** | 输入频道 + 关键字，服务端匹配，能翻到频道全部历史，不受“最近 N 条”窗口限制 |
| **勾选下载** | 搜索结果列出标题 / 大小 / 发布时间 / 帖子正文，勾一条或全选，再统一排队 |
| **多任务队列** | 独立的下载页，可限制“同时下载文件数”，9 个视频一起排，按序推进 |
| **多段并发** | 单文件切成 N 个 512 KiB 段落并行拉取，每段写自己那一段偏移，互不重叠 |
| **断点续传** | `xxx.mp4.part` + `xxx.mp4.part.tgvd`（逐段已完成字节数），暂停/关窗/断网后从断点继续 |
| **暂停与继续** | 「开始」「全部开始」恢复已停止 / 失败 / 未完成的任务；正在下载和已完成的自动跳过，不会产生两个写入者 |
| **自动命名** | 原文件名可用就用原文件名（纯日期这种垃圾名会被丢掉）；否则取帖子正文首行，并从 `#《剧名》` 这类标签里抠剧名；再兜底 `频道_消息ID` |
| **进度信息** | 名称 / 大小 / 进度 / 状态 / 下载速度 / 剩余时间 / 连接数，速度做了指数平滑，ETA 不会乱跳 |
| **单文件绿色版** | 静态链接，~7 MB，无控制台窗口，不依赖 .NET / VC 运行库 |

## 使用

**不想自己编译**：直接到 [Releases](https://github.com/KingNuoNuo/tgvd/releases/latest) 下 `TGVD-<版本>-windows-x64.exe`，双击就能用。也可以从 [Actions](https://github.com/KingNuoNuo/tgvd/actions/workflows/build.yml) 里随便挑一次成功构建，下载 artifact 的压缩包。

1. **准备 api_id / api_hash**：到 <https://my.telegram.org> 建一个应用。程序里预填了一组公开的桌面端参数，能直接登录；长期用建议换成自己的。
2. **登录**：填手机号（国际格式 `+86…`）→ 收验证码 → 有二步密码的再输一次。凭证存在 `%APPDATA%\TGVD\tgvd.session`，之后启动自动恢复，换 exe / 换目录都不掉登录。
3. **搜索**：侧栏「搜索」→ 填频道（`@channel` 或 `t.me/channel`）→ 可选关键字 → 搜索 → 勾选 → 「下载选中」。
4. **下载**：侧栏「下载」看队列。停止、开始、重试、打开文件夹、复制链接都在底部工具栏。

### 文件位置

| 路径 | 内容 |
|---|---|
| `%APPDATA%\TGVD\tgvd.session` | 登录凭证（auth key），**别外传** |
| `%APPDATA%\TGVD\settings.json` | 输出目录、同时下载文件数、每文件分段数、单频道取多少条、深色模式 |
| `%APPDATA%\TGVD\tgvd.log` | 运行日志，带 `[+Nms]` 耗时前缀，超过 256 KB 自动清空 |
| 输出目录 | 默认 `~/Downloads/TGVD`，下载中写 `.part`，全部字节到齐才改名成正式文件 |

## 构建

### 云端构建（不装任何东西）

推到 GitHub 就自动编，配置在 `.github/workflows/build.yml`：

- 推 `main` 或手动点 **Run workflow** → 编好放在 Actions 的 artifact 里（保留 30 天）
- 推 `v*` tag → 编完自动建 Release 并把 exe 挂上去

用的是 `windows-latest` 自带的 MSVC 工具链，加 `-C target-feature=+crt-static` 静态链接 CRT，所以产物不挑机器、不需要装 VC++ 运行库。

### 本地构建

需要 Rust 1.75+。Windows 上 MSVC 或 GNU 两种工具链都可以：

```bash
# MSVC（装了 Visual Studio Build Tools）
cargo build --release

# GNU（用 MinGW-w64；本项目开发时用的是解压到 tools/mingw 的 winlibs 包，
#      需要时把 .cargo/config.toml.example 复制成 .cargo/config.toml）
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

产物是 `target/<triple>/release/tgvd.exe`，单文件，直接双击。

关闭默认 feature 是刻意的：`grammers-session` 的 sqlite 存储会拖进 libsql（一个捆绑的 C SQLite），这里改成自己用 JSON 落盘，换来一个干净的单文件 exe。

## 技术说明（踩过的坑，写下来省得下次再挖）

- **每个数据中心只有一条 TCP 连接**。`grammers-mtsender` 的 `SenderPoolRunner` 按 `dc_id` 复用连接（`connections.iter().find(...)`），所有分段请求都挤在这一条上排队。所以分段数从 4 加到 16 提升有限，继续加反而更糟。
- **并发过高会塌**。实测 3 个文件 × 16 段 = 48 个 `getFile` 请求同时在路上时，服务端只回了最前面一批就不再回，吞吐从 60 MB/s 掉到 30 KB/s，而 TCP 连接没断、零重传、日志零报错。单文件 16 段是安全的。
- **`iter_download` 是一问一答**：发一块 512 KiB，等到回包再发下一块，没有流水线。要快只能多开几条流。
- **不能用 aria2**：Telegram 媒体走的是 MTProto 的 `upload.getFile` RPC，不是 HTTP URL，aria2 只认 HTTP/FTP/BT。
- **别用 `set_len(size)` 预分配**：`grammers` 对 > 10 MB 的文件会先把文件撑到完整大小，进度条第一帧就 100%（文件里全是 0）。这里统一写 `.part`，全部字节到齐才 rename。
- **别拿 `.part` 的文件大小当进度**：多段并发写的是不同偏移，文件大小等于“最高那一段的起点”，会虚高。真实进度看 `.tgvd` 里每段已完成字节之和。

## 已知限制

- 只处理 `video/*` 类型的 document，纯文字 / 图片 / 音乐不下载。
- 任务列表不持久化：关掉窗口要重新搜索添加，但 `.part` 和 `.tgvd` 还在，同名文件再加一次会接着续传。
- 同一份 `tgvd.session` 同时开多个实例会互相干扰，测速时只留一个窗口。
- 只支持 Windows（`explorer /select` 之类是 Windows 调用）。

## 调试开关

没有控制台也能查问题，几个环境变量就够了：

| 变量 | 作用 |
|---|---|
| `TGVD_DIAG=1` | 把每条消息的原始文件名 / 分组 / 配文、以及最终命名写进 `tgvd.log` |
| `TGVD_PROBE=@channel` | 登录后自动跑一次搜索（只读，不下载） |
| `TGVD_PROBE_DL=1` | 配合上面，自动下载第一个结果，用于端到端验证 |
| `TGVD_DEMO=1..5` | 假数据：1 下载页、2 添加弹窗、3 设置页、4/5 搜索页，用来检查排版 |

## 关键词

Telegram 视频下载 · 电报频道下载器 · 纸飞机下载工具 · t.me 视频批量下载 · MTProto 下载 · 频道内搜索下载 · 断点续传下载器 · Rust GUI 下载器 · egui 桌面应用 · Windows 单文件下载工具

## License

MIT

---

### English

**TGVD** is a single-file Windows GUI that downloads videos from Telegram channels over **MTProto** using your own account — no public web-preview scraping, so channels that block anonymous previews still work.

Search inside a channel by keyword, tick the videos you want from the result list, and queue them. Downloads run as multiple 512 KiB segment streams with per-segment resume state (`*.part` + `*.part.tgvd`), a task queue with a configurable file limit, pause/resume, and automatic file naming derived from the original file name, the post caption, or `#《title》` hashtags.

Built with Rust, `eframe`/`egui` and `grammers` (pure-Rust MTProto). No external runtime, no console window, ~7 MB, statically linked.

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

Prebuilt binaries are produced by GitHub Actions on every push (`.github/workflows/build.yml`, MSVC + static CRT) and attached to each release, so end users can skip the toolchain entirely.

> Note: Telegram media is delivered through the `upload.getFile` RPC, not HTTP — that is why aria2 or any HTTP downloader cannot be used as the backend. Each datacenter also gets a single TCP connection from `grammers-mtsender`, which caps how much parallelism actually helps.
