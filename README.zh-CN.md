# Muxloom · 中文指南

<p align="center">
  跨本地与 SSH 机器、可持续存活的 Codex、Claude Code 与 Shell 会话终端工作台。
</p>

<p align="center">
  <a href="./README.md">English</a> ·
  中文 ·
  <a href="https://github.com/MarsTechHAN/Muxloom/releases">Releases</a>
</p>

**Homebrew（macOS）**

```bash
brew tap marstechhan/muxloom https://github.com/MarsTechHAN/Muxloom && brew install --cask muxloom
```

> [!NOTE]
> 本中文文档由英文 [README](./README.md) 翻译而来，可能落后于最新英文版；如有出入，以英文版为准。

---

### 目录

- [项目概览](#zh-overview)
- [安装](#zh-install)
- [首次使用](#zh-first-run)
- [界面与布局](#zh-interface)
- [操作](#zh-controls)
- [配置](#zh-configuration)
- [MCP](#zh-mcp)
- [会话、历史与提醒](#zh-sessions)
- [文件管理与预览](#zh-files)
- [实现架构](#zh-architecture)
- [排障](#zh-troubleshooting)
- [限制与安全](#zh-limitations)

<a id="zh-overview"></a>

### 项目概览

Muxloom 是一个用 Rust 实现的终端工作台，用于从一个 TUI 管理分布在多台机器、多个
目录中的 Codex、Claude Code 和普通 Shell 会话。它提供：

- 从 `~/.ssh/config` 加载 SSH 目标，并同时管理本机；
- 按机器、目录分组或 Flat 模式浏览会话；
- 在 Dashboard 内完整渲染并交互真实 PTY；
- Resume、Recap、Archive、交互提醒和跨会话全文搜索；
- 本地/远程文件浏览、结构化文本、图片和视频预览、下载与拖拽上传；
- 横屏、竖屏和小尺寸 Compact 布局，以及独立持久化的分割位置。

正常会话由目标机器上的 `muxloomd` 持有，不会出现在 `tmux ls`。退出 Dashboard、
SSH 断开或 Controller 休眠都不会结束 Agent。正常 daemon 数据面为每台远端机器维护
一条持久 SSH Bridge，Control、PTY、History、Files 和编码媒体数据都复用这条连接。
Bootstrap 和明确选择的兼容路径可能临时复用单独的 SSH ControlMaster 或 `scp`。

平台支持：Linux x86_64、Apple Silicon macOS 和 Intel macOS 可以运行 Controller 与
本地会话；Windows x86_64 可以作为 SSH Controller，暂不支持 Windows 本地 daemon。

<a id="zh-install"></a>

### 安装

通过上游 tap 安装完整发布包，其中包括配套 daemon、companion 二进制与媒体工具：

```bash
brew tap marstechhan/muxloom https://github.com/MarsTechHAN/Muxloom
brew install --cask muxloom
```

从 [GitHub Releases](https://github.com/MarsTechHAN/Muxloom/releases) 下载对应控制机的
压缩包。请保留解压后的目录结构，`muxloom` 会相对自身查找 `muxloomd`、其他目标架构
的 companion 和 FFmpeg。

macOS/Linux：

```bash
chmod +x muxloom muxloomd ffmpeg companions/*/muxloomd
./muxloom init
./muxloom
```

Windows 运行 `muxloom.exe` 管理 SSH 目标。每个压缩包和独立 companion 都带有
`.sha256` 文件。

源码构建需要 Rust 1.85 或更高版本；远程目标需要 `ssh`；视频预览需要 `PATH` 中的
`ffmpeg` 或 `MUXLOOM_FFMPEG`。Controller 已内置 HTTPS 下载、SHA-256 校验和压缩包
解压，补取 companion、Agent Package 和自更新都不依赖系统 `curl` 或 `tar`。

```bash
git clone https://github.com/MarsTechHAN/Muxloom.git
cd Muxloom
cargo build --release
./target/release/muxloom init
./target/release/muxloom
```

开发运行使用 `cargo run --bin muxloom`。

命令行：

```text
muxloom [--config PATH] [--debug | --debug-log PATH]
muxloom init [--config PATH]
muxloom update [--config PATH]
muxloom mcp [--config PATH]
```

`--config` 指定 TOML；`--debug` 写入默认 Debug Log；`--debug-log PATH` 指定日志路径。
`muxloom init` 不会覆盖已有配置。
`muxloom update` 会校验 SHA-256 后原地更新 Release Bundle。启动时默认在后台自动检查并
更新；配置 `auto_update = false` 可以关闭。默认情况下（`update_prompt = "ask"`）发现
新版本会先弹窗询问：安装包构建可原地更新，源码构建则把远端机器要用的 muxloomd
companion 拉进本地缓存；`"auto"` 恢复静默自动更新，`"never"` 跳过启动检查。GitHub
不可达时 companion 部署会降级使用已校验的本地缓存并明确提示可能过期。

<a id="zh-first-run"></a>

### 首次使用

1. 启动 `muxloom`。本机默认启用，具体 SSH Alias 会显示在 Machines。
2. 单击选择远端机器，按 `Space` 或在 `[x]` 上双击启用；未启用的机器不会探测。
3. 按 `n`，启动目标就是当前选中的机器。
4. 选择 Codex、Claude 或 Terminal，选择工作目录，并可填写 Label。
5. 选择 `New session`，或从当前精确目录下同时扫描出的 Codex/Claude 历史中 Resume/Reference。
6. 按 `Enter` 或点击 Terminal 开始输入。
7. 使用平台组合键加方向键，或点击 Back 返回导航。
8. 按 `q` 退出 Dashboard；daemon 管理的会话继续运行。

路径选择器支持直接输入文本，排序依次为前缀、子串、前向子序列匹配。`Left` 返回父目录，
`Right` 进入子目录，`Enter` 确认当前目录。Resume 页面也支持 `Left` 返回启动表单。

Codex Resume 来源是 `~/.codex/sessions` 和 `~/.codex/session_index.jsonl`；Claude
来源是 `~/.claude/projects`。每次都会扫描两者并用 Runtime 图标标记。相同 Runtime 原生
Resume；交叉选择会先提示类型不匹配，确认 Reference 后新建目标 Agent，并在首条 Prompt 中
给出源 History 文件。候选项优先显示 Recap，没有 Recap 时显示第一条和最后一条用户消息。
Terminal 始终新建。切换机器时，每台机器会恢复上次选中的 Agent。

<a id="zh-interface"></a>

### 界面与布局

横屏：

```text
┌──── Machines ────┬──── Agents by folder ────┬──────── Live terminal ────────┐
│ 机器与 Runtime   │ 会话、目录、Recap、状态  │ Codex、Claude、Shell 或预览  │
└──────────────────┴───────────────────────────┴───────────────────────────────┘
```

竖屏：

```text
┌────────────────────────── Live terminal ───────────────────────────┐
│ Terminal 始终位于上方并占大部分空间                               │
├──────────────── Machines ─────────────┬──── Agents by folder ─────┤
│ 左下                                  │ 右下                       │
└───────────────────────────────────────┴────────────────────────────┘
```

选中的侧栏会自动变宽。横屏的 Machine/Agent 宽度、竖屏的 Terminal 高度和下方分割点
分别记录。终端尺寸过小时使用按 Focus 展示的 Compact 模式；Terminal 可以占满内容区域，
通过组合键或 Back 返回。

会话状态：

| 状态 | 含义 |
| --- | --- |
| Working | 当前可见 Codex/Claude 终端被启发式识别为正在推理或执行工具 |
| Waiting | 当前可见屏幕正在等待确认或输入 |
| Idle | 进程存活并停在普通输入提示 |
| Archived | Agent 已退出或被停止，元数据和历史仍保留 |

Working 动画同时出现在 Machines 和 Folder/Agent 行：Codex 使用青色旋转盲文 spinner
（`⠋⠙⠹…`），Claude 使用橙色 sparkle（`✻✽✶✳`），与 Claude Code 自身循环的字形一致。
两者均按墙钟时间推进，速度不随重绘频率变化。Terminal、Idle、Waiting、Archived
不播放动画。Codex Working 直接跟随 OSC 标题 spinner，因此 CLI 擦除并逐字符重绘可见
`Working` 行时也不会漏判；Waiting Agent 的整个条目会变成黄色加粗。

<a id="zh-controls"></a>

### 操作

底部 Footer 会根据当前上下文展示常用操作；按 `?` 打开完整分类 Help。

| 按键 | 行为 |
| --- | --- |
| macOS `Cmd+Arrow` / `Option+Arrow` | 按实际布局移动 Focus |
| Windows/Linux `Alt+Arrow` | 按实际布局移动 Focus |
| `Alt-1` / `Alt-2` / `Alt-3` | 跳到 Machines / Agents / Terminal |
| `Space`（Machines） | 启用或禁用机器 |
| `n` / `Ctrl-n` | 在当前机器进入 New/Resume 流程 |
| `t`（Agents） | 选择 Codex 或 Claude，在当前目录启动无历史的 Temporal Chat |
| `p`（Agents） | 为所选机器设置本地端口转发 |
| `Enter` | 打开 Terminal 或确认表单 |
| `x` | Live Agent 归档；Temporal Chat 直接销毁；Archived Agent 永久删除 |
| `a` | 展开或收起 Archived |
| `/` / `Ctrl-p` | 搜索全部会话历史 |
| `Ctrl-f` | 按当前上下文展开或关闭 Files |
| `,` / `Ctrl-,` | 编辑当前机器配置 / 全局配置 |
| `f` | 切换 Grouped / Flat |
| `v` / `Ctrl-h` | 隐藏未启用机器 / 显示全部 |
| `r` / `Ctrl-r` | 立即刷新 |
| `q` | 退出 Dashboard，不停止 Agent |

所有列表都停在第一项和最后一项，不会循环；鼠标滚轮每次只移动一项。

Terminal 输入激活后，普通方向键、文字、粘贴和组合键直接进入 PTY；未加 Modifier 的方向键
不会切换 Pane。`Shift+Enter` 或 `Option+Enter` 插入换行，`Ctrl-c`/`Ctrl-d` 原样转发。
`PageUp`/`PageDown` 按页浏览历史，滚轮每次移动一行。已连接会话的回滚读取仿真器自身渲染的
scrollback，因此 Codex、Claude Code 等实时重绘 TUI 显示真实行而非线性化日志。attach 时
即可回滚数千行：这些行由 daemon 从会话日志渲染而来，因此即便 Agent 把保留输出几乎都花在
重绘帧上、真正完成的行寥寥无几，重新启动 controller 之后仍有足够的历史可以翻。回滚时以及
打开文件浏览器时都可以选择并复制内容。

鼠标支持点击 Focus/选择、Archive、Back 和提醒 Banner。Machine 单击只选择；只有在 `[x]`
范围内双击才会启用或禁用。可以拖动所有布局分隔线；
直接拖选 Terminal 文本会在松开时复制，`Alt+拖拽` 则转发给启用 Mouse Reporting 的程序。

<a id="zh-configuration"></a>

### 配置

默认配置是 `~/.config/muxloom/config.toml`；文件不存在时使用内置默认值。启用机器、
布局分割点、Grouped/Flat 和 Archive 可见性单独保存在
`~/.local/state/muxloom/state.json`。

完整 TOML 示例见英文部分的[配置](#en-configuration)。主要配置包括：

- `refresh_interval_ms`、SSH Timeout、History 大小和分段行数；
- SSH Config 路径、全局 Attention Patterns；
- Codex、Claude、Terminal 的 `command`、`args`、安装命令与 `sync_files`；
- 全局和每台机器独立的 `NAME=value` 环境变量；
- 每台机器独立的 `REMOTE_PORT:LOCAL_HOST:LOCAL_PORT` Reverse Tunnel；
- companion 命令和可选 Controller 本地 binary 路径；
- `[hosts.<alias>]` 下对某台机器的 Runtime 覆盖。

`command` 是单个可执行文件名或路径，参数作为结构化数组传输。Pipe、Redirect 或复杂初始化
应写入 Wrapper。环境变量使用下面的非 JSON 格式，并默认注入 Install 和 Launch；legacy
Shell Probe 也会注入，但 daemon-native executable probe 不会：

```text
HTTP_PROXY=http://proxy:8118 HTTPS_PROXY=http://proxy:8118 TOKEN='two words'
```

选择机器按 `,` 编辑该机器的有效配置；按 `Ctrl-,` 编辑全局默认值。Args、Sync Files 和
Attention Patterns 使用 Shell Word 语法。

当 New 发现 Codex/Claude 不存在时，Muxloom 会先询问。它优先复用 Controller 上兼容的
binary，否则由 Controller 下载并校验目标平台产物，再传到目标；这条 staging 路径不要求
目标机器访问外网。Controller 无法准备时才执行该机器配置的用户态安装命令，此时默认
Installer 需要目标直连或通过 Reverse Tunnel 访问网络。`sync_files` 会复制到目标用户 Home
下相同的相对路径，已有文件先备份，历史目录不会作为配置同步。

<a id="zh-mcp"></a>

### MCP

两个 binary 都能通过 Model Context Protocol（stdio 传输）把整个工作台交给 AI Agent 操作：

- **`muxloom mcp`** —— Controller 面，headless 运行。读取与 Dashboard 相同的配置和状态，
  可触达所有**已启用**的机器：列出机器与会话（含实时 working / needs_attention 状态与
  Recap）、启动与恢复会话、向会话输入、读取渲染后的屏幕与回滚、全文搜索历史、浏览与预览
  文件、归档或删除会话、执行 Shell 脚本。TUI 不需要在运行。
- **`muxloomd mcp`** —— 同样的工具形态，但只作用于本机 daemon，适合运行在目标机器上的
  Agent。每次调用都用一条短连接访问本机 `muxloomd` socket（daemon 未运行时会自动拉起），
  因此挂着的 MCP 客户端不会推迟 daemon 升级。

在 Claude Code 中注册：

```bash
claude mcp add muxloom -- muxloom mcp
# 或在承载会话的机器上使用 daemon 面：
claude mcp add muxloomd -- muxloomd mcp
```

Codex（`~/.codex/config.toml`）：

```toml
[mcp_servers.muxloom]
command = "muxloom"
args = ["mcp"]
```

Agent 驱动 Agent 的典型流程：`list_sessions`（或 `launch_session`）→ `send_input` 带
`submit: true` 提交 Prompt → 轮询 `list_sessions` 等 `working` 结束（`needs_attention`
会带上命中的审批提示）→ `read_screen` 读取结果。

通过 MCP 启动的会话就是普通托管会话：会出现在 Dashboard 中、在 MCP 客户端退出后继续运
行，也要像其他会话一样归档或删除。未启用的机器不会被触碰——指向它的调用会被拒绝。

工具面本身与传输无关（`src/control.rs`）；MCP stdio 是它的第一个适配器，同一接缝将来
可以接 TCP 或串口适配器，让硬件状态面板读取 Agent 状态。

> [!WARNING]
> `run_shell` 与 `send_input` 允许连接的 MCP 客户端在启用机器上以你的用户身份执行任意
> 命令，请据此决定向哪些客户端开放。

<a id="zh-sessions"></a>

### 会话、历史与提醒

`muxloomd` 直接持有 PTY 和子进程，Dashboard 与 SSH Bridge 只是订阅者。重连后按 Session
ID 恢复订阅。daemon 在目标端追加保存 ANSI History；旧页面按需分段读取并缓存在本机，
有效大块使用 LZ4。Offset 会限制在真实历史内，不会滚到不存在的空白区域。

历史渲染保留基础色、256 色、Truecolor 前景/背景以及 Bold、Dim、Italic、Underline、
Reverse 和 Crossed-out 属性。

Recap 先取最后一个 `※ recap:`（也支持全角冒号）；否则取最后一条能识别的 Codex/Claude
Assistant 行并排除工具/状态行；仍无法识别时，回退到最后一条非界面文本。结果会归一化
控制字符和空白，并限制在 180 字符。按 `/` 或 `Ctrl-p` 搜索 Live/Archived、本地/远端
全部会话，排序优先级是 Label/名称/路径、当前 Recap 与 Recap History、其他 History。

Codex/Claude 退出或第一次按 `x` 后进入 Archived，仍可查看和搜索。打开 Archived 会按
原机器、Runtime 和目录尝试 Resume 最新历史。确认框默认勾选在新 Agent 成功启动后移除旧
Archived 条目，按 `Space` 可选择保留，该选择会持久记忆；启动或清理失败时旧归档保持不变。
再次按 `x` 才永久删除 daemon 元数据与历史。
普通 Terminal 不归档，Shell 退出或按 `x` 后直接清理。

在 Agents 面板按 `t` 会先选择 Codex 或 Claude，再启动 `Temporal Chat`。目录依次取当前选中
Agent 的目录、该机器上次启动目录、目标用户 Home；该会话不写 Muxloom ANSI History，也不
进入搜索或备份；Codex 还会用单次配置关闭 Transcript 持久化。按 `x` 会直接停止并删除，
不进入 Archived。

在 Agents by folder 按 `p` 可把所选机器上的服务转发到 Controller 的 Loopback。填写远端
Host/Port 与本地 Port（`0` 表示自动分配），之后访问 `127.0.0.1:LOCAL_PORT`。Linux companion
会原生探测非特权监听端口；所有平台也会从 Agent 当前终端中可见的 Loopback URL 提取候选。
探测不可用时仍可手动填写。TCP 流量复用该机器已有的持久 Bridge；选中活动转发按 `d` 停止，
不会停止远端服务或 Agent。本地 Listener 只在当前 Muxloom Controller 进程期间存在。

提醒只检查当前屏幕底部物理行。Attached 和 legacy-inspected Session 会组合内置审批布局与
每机器 Pattern；后台 daemon snapshot 当前只使用内置布局。新提醒会把整个 Agent 条目显示为
黄色加粗，并显示可点击 Banner、Waiting 状态、Bell 和 OSC 9，对同一个 Prompt 去重。进入会话即消除它的 Banner——会话
列表仍然显示 Waiting，直到 Agent 不再询问——之后的新 Prompt 会重新提醒。Attached
Terminal 直接从实时帧更新 Working/Waiting，Codex 还读取持续变化的 OSC 标题 spinner，
因此短推理和可见状态行重绘不会被后台刷新间隔漏掉；其他会话由 daemon snapshot 更新。

<a id="zh-files"></a>

### 文件管理与预览

按 `Ctrl-f` 从当前 Agent 目录打开 Files；没有选中会话时从当前机器的 `.` 开始。

- Focus 在 Live Terminal 时，Files 作为左侧 Sidebar，右侧保留 Terminal；打开文件后右侧
  变为 Preview，再打开一次恢复 Terminal。
- Focus 在 Agents by folder 时，该 Pane 变成文件选择器；打开文件后在上方/右侧的大
  Terminal Pane 中显示 Preview。

Focus 在文件 Pane 时它会捕捉文本和组合键，`n` 等不会泄漏成全局操作。把 Focus 切到其他
Pane（Pane-focus 快捷键或点击）即可在那里输入或操作，Files 仍停留在自己的 Pane。进入或
离开目录会清空 Match Query。

| 按键或操作 | 行为 |
| --- | --- |
| `Up` / `Down`、`j` / `k` | 选择文件或目录 |
| 直接输入 | 在当前目录 Match |
| `/pattern` | 递归搜索当前目录下的文件名，支持 `*` 和 `**` 通配符 |
| `Right`、`Enter`、双击 | 进入目录或展开/收起 Preview |
| `Left`、右键 | 返回父目录 |
| 方向键、`PageUp` / `PageDown` | 对打开内容翻页，并停在开头或末尾 |
| `g` / `G`、`Home` / `End` | 跳到 Preview 开头或末尾；停在末尾时会随文件增长自动跟随 |
| `c` | 复制目标机器上的完整路径 |
| 在 Preview 上拖拽 | 松开鼠标时复制选中的 Preview 文本 |
| `d` | 下载到 Controller 的 `~/Downloads` |
| 拖入本地文件 | 上传到当前浏览目录 |
| `r` / `F5` | 重新读取打开的 Preview；未打开时刷新目录 |
| 拖动 Files 分隔线 | 调整并保存文件 Pane 宽度 |
| `Esc` | 依次关闭 Preview、清空 Query、关闭 Files |
| `Ctrl-f` | 从任意文件状态直接关闭 Files |

目录枚举、内容识别、读取和元数据提取都在目标执行。Loading 不阻塞返回父目录或进入已缓存
目录；过期请求结果会丢弃，并预加载相邻目录和文件。

Preview 不再截断：一次响应装不下的正文会通过分块文件流补齐，并且只把屏幕上的行渲染成带样式
的文本，因此翻阅几 MB 的日志和翻阅小文件每帧开销相同。

打开中的文件会被持续监视：目标上的改动几秒内就会显示，无需关闭再打开；视图停在末尾时会随
文件增长自动翻到新的末尾。监视只传目录条目的元数据，仅当大小或修改时间变化才重新读取文件。
超过 4 MiB 的文件不会自动重读，按 `r` 或 `F5` 主动拉取最新内容。

源码和普通文本按内容识别并使用 `syntect` 高亮；Markdown 支持 `#` 到 `####`、Bold、列表、
引用、代码块、Table 和 `---`；JSON、JSONL、CSV、TSV 会结构化解析，其中 CSV/TSV 会给行列
编号，有表头时表头固定在顶部，翻页也不会滚走。
图片在 Controller 用 Rust 解码并以 Truecolor 半块显示；视频保持编码形式跨 Bridge，在
Controller 用 FFmpeg 解码和按时间渲染。目标不需要 FFmpeg，也不会传膨胀的 RGB。Audio
目前只显示元数据，不播放。

打开 Preview 不会产生本地副本。只有下载才落盘，并显示字节数、百分比和实时速度。

<a id="zh-architecture"></a>

### 实现架构

参见英文部分的[架构图](#en-architecture)。主线程负责 Crossterm Event、`App` 状态和
Ratatui 绘制；文件、搜索、Probe 和 Runtime 操作通过强类型 Request/Event 在 Worker 执行。

每个远端目标的正常 daemon 数据面使用一条不分配 PTY 的 SSH 长连接。Versioned Frame
使用 Request ID 和 Stream ID 复用并发操作、PTY、文件、媒体与 TCP 转发，Credit Window 提供背压，Heartbeat 检测
断线，大数据只在值得时使用 LZ4。daemon Bridge 的 Reverse Tunnel 作为同一 SSH 进程的
`-R` 参数；legacy 和 staging 兼容路径仍可能使用 ControlMaster 或 `scp`。

Bootstrap 由 Rust binary 自己计算 SHA-256 Fingerprint。缺失或过期 companion 通过同一
SSH stdin 原子安装。如果 daemon provisioning 或 launch 失败且目标有 tmux，可以进入明确
标记且必须确认的兼容回退。

每个会话由一个极小的 **keeper** 进程持有：只负责 PTY、子进程和原始历史追加，协议永久
冻结，因此它本身几乎不需要更新。daemon 只是 keeper 的当前客户端——负责屏幕、状态、
搜索与元数据。daemon 升级不再等待空闲：换代时会话由各自的 keeper 原地带过去，新
daemon 连上 keeper socket 即收养（同一进程、同一转录），daemon 崩溃也不再杀死会话。
运行中的 daemon 落后于当前构建时，footer 右下角会出现 `⟳` 标记，Controller 会在该机
终端未 attach 时自动重连完成升级。History 和 Metadata 始终保留在状态目录。

Terminal 字节由 `vt100::Parser` 维护 Alternate Screen、光标、颜色、样式、Mouse Mode、
Application Cursor 和 Bracketed Paste，并由 Ratatui 限制在对应 Pane 内。Muxloom 会用
`--no-alt-screen` 启动 Codex，让对话记录进入回滚缓冲，而不是在整屏重绘时丢失；该设置只
影响由 Muxloom 启动的 Codex 进程。切换 Agent 时保留旧画面，后台等待新 PTY 首帧后再原子
替换，避免闪白。

主要模块职责见英文部分的 [Source map](#en-architecture)。

<a id="zh-troubleshooting"></a>

### 排障

```bash
muxloom --debug-log /tmp/muxloom-debug.log
```

重点日志：

- `layout ... portrait=... compact=...`：实际布局判断；
- `probe done ... backend=muxloomd`：目标和 daemon Session 扫描成功；
- `source=live-terminal ... working=...`：Attached 实时帧改变状态；
- `source=muxloomd ... working=...`：daemon 返回的其他会话状态；
- `terminal first frame ready`：新 Terminal 首帧已切换；
- `bridge reached EOF`：连接关闭，但 daemon Agent 可能仍在运行。

常见检查：

- Machine Offline：先执行 `ssh -T -o BatchMode=yes <alias> true`，再看 Bootstrap 错误；
- Remote 能显示不能输入：确认 persistent bridge、first frame ready，且随后没有 EOF；
- Working 动画不出现：检查 `source=live-terminal` / `source=muxloomd` 和 companion Fingerprint；
- Codex 缺 `bubblewrap`/`bwrap`：使用带资源的 Standalone Package 或绝对 Wrapper；
- 竖屏仍横向：查看 Layout Log 的 Pixel/Cell，外层终端可能没有报告 Pixel Size；
- 提醒误报：根据 Reason 和可见 Tail 收窄该机器的 `attention_patterns`；
- footer 出现 `⟳` 标记：该机器运行中的 daemon 落后于当前构建；终端未 attach 时
  Controller 会自动完成升级，会话不受影响；
- 视频不能解码：检查 Bundle 内 FFmpeg、`MUXLOOM_FFMPEG` 或 Controller `PATH`。

Debug Log 可能包含少量当前 Agent 可见文本，应按敏感信息处理。

<a id="zh-limitations"></a>

### 限制与安全

- Codex 与 Claude 私有 History 格式不同；跨 Runtime Reference 会新建会话并在首条 Prompt
  中引用源 History 文件，不会伪装成原生 Resume 或转换私有格式；
- Windows 暂时只能作为远程 Controller；
- Audio Playback、Video Seek 和音量控制暂未实现；
- Resume 依赖 Codex/Claude 当前的本地元数据格式；
- Attention 是启发式检测，每台机器的 Pattern 应尽量具体；
- 启用机器意味着允许周期性 BatchMode SSH 和 companion 管理；
- 目标 History、Debug Snippet 和搜索结果都可能包含敏感内容；
- 连接 `muxloom mcp` / `muxloomd mcp` 的 MCP 客户端可以读取历史、向会话输入并以你的用户
  身份在启用机器上执行 Shell 脚本；
- Muxloom 默认不添加跳过 Agent 权限检查的参数，用户配置的 Runtime Args 仍具有对应风险。

---

## License

Muxloom 依据 [GNU General Public License v3.0 only](./LICENSE) 分发。
