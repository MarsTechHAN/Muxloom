# Muxloom · 中文指南

<p align="center">
  跨本地与 SSH 机器、可持续存活的 Codex、Claude Code、OpenCode、Pi 与 Shell 会话终端工作台。
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
- [触摸屏](#zh-touch)
- [配置](#zh-configuration)
- [MCP](#zh-mcp)
- [Agent 协作层](#zh-collaboration)
- [会话、历史与提醒](#zh-sessions)
- [文件管理与预览](#zh-files)
- [实现架构](#zh-architecture)
- [排障](#zh-troubleshooting)
- [限制与安全](#zh-limitations)

<a id="zh-overview"></a>

### 项目概览

Muxloom 是一个用 Rust 实现的终端工作台，用于从一个 TUI 管理分布在多台机器、多个
目录中的 Codex、Claude Code、OpenCode、Pi 和普通 Shell 会话。它提供：

- 从 `~/.ssh/config` 加载 SSH 目标，并同时管理本机；
- 按机器、目录分组或 Flat 模式浏览会话；
- 在 Dashboard 内完整渲染并交互真实 PTY；
- Resume、Recap、Archive、交互提醒和跨会话全文搜索；
- 本地/远程文件浏览、结构化文本、图片和视频预览、下载与拖拽上传；
- 横屏、竖屏和小尺寸 Compact 布局，以及独立持久化的分割位置；
- 一层跨机同步的 Agent 协作面：共用公告板、Agent 之间的私信、等待与触发器。

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

cask 走的是打了 tag 的正式版：预编译好的二进制，muxloom 可以原地自更新。想跟随
`main` 就装 nightly formula，它在本机现编译 Controller 和配套 daemon，而不是下载：

```bash
brew install --HEAD marstechhan/muxloom/muxloom-nightly
```

两者都提供 `muxloom` 命令，同一时间只保留一个链接。

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

**Nightly**：`main` 上每个通过全平台回归的 commit 都会重新发布为滚动预发布
[`nightly`](https://github.com/MarsTechHAN/Muxloom/releases/tag/nightly)，修复当天即可
安装。已有安装只需执行：

```bash
muxloom update --nightly
```

之后无需任何配置即可留在 nightly：装上的构建本身带着 nightly 标记，而每个安装默认
跟随自己所属的那条线，因此后续检查继续推送 nightly。`muxloom update --stable` 用同样
的方式回到正式发布。首次安装可以直接从 nightly 页面下载压缩包，目录结构与打了 tag 的
发布完全一致。

Homebrew 用编译的方式走同一条线：

```bash
brew install --HEAD marstechhan/muxloom/muxloom-nightly
brew upgrade --fetch-HEAD muxloom-nightly
```

在本机运行的东西都从 `main` 现编译；本机编不出来的——其他架构的 companion——仍由
muxloom 在某台机器第一次需要时从发布页取。这些文件归 Homebrew 所有，因此
`muxloom update` 只会报告有更新的 nightly，安装交给 `brew upgrade`。

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
muxloom update [--config PATH] [--nightly | --stable]
muxloom mcp [--config PATH]
```

`--config` 指定 TOML；`--debug` 写入默认 Debug Log；`--debug-log PATH` 指定日志路径。
`muxloom init` 不会覆盖已有配置。
`muxloom update` 会校验 SHA-256 后原地更新 Release Bundle。启动时默认在后台自动检查并
更新；配置 `auto_update = false` 可以关闭。默认情况下（`update_prompt = "ask"`）发现
新版本会先弹窗询问：安装包构建可原地更新，源码构建则把远端机器要用的 muxloomd
companion 拉进本地缓存；`"auto"` 恢复静默自动更新，`"never"` 跳过启动检查。
发布分两条线：打了 tag 的正式版，以及上面说的滚动 `nightly`。`update_channel` 决定
检查哪一条，默认值 `"auto"` 跟随当前运行的这个构建所属的那条——正式版安装继续按大版本
走，nightly 安装继续拿 nightly，谁都不会被拖到没要过的节奏上；`"nightly"` 与
`"stable"` 则直接指定。nightly 在弹窗里显示为 `nightly <版本>+<commit 数> (<commit>)`，
并列出当前运行的构建，因为两个 nightly 往往只差那个 commit 数；只有确实比当前更新的
nightly 才会被提示。`-V` 会打印 CI 构建对应的 commit 与所属线。
GitHub 不可达时 companion 部署会降级使用已校验的本地缓存并明确提示可能过期。设置面板
（`,` 机器级 / `Ctrl-,` 全局）按分组只展示常用项——刷新间隔、环境变量、各 agent 命令、
更新提示与更新通道；隧道、companion 覆盖、安装命令、sync files、attention patterns 等
低频项仅在 config.toml 中配置。机器级面板还会显示该机 `muxloomd` 的运行版本，并带一个
**Force update** 动作，用来强制完成被旧会话拖住的 daemon 升级。

<a id="zh-first-run"></a>

### 首次使用

1. 启动 `muxloom`。本机默认启用，具体 SSH Alias 会显示在 Machines。
2. 单击选择远端机器，按 `Space` 或在 `[x]` 上双击启用；未启用的机器不会探测。
3. 按 `n`，启动目标就是当前选中的机器。
4. 在该机器已安装的 Runtime 中选择一个，选择工作目录，并可填写 Label。
5. 选择 `New session`，或从当前精确目录下同时扫描出的 Codex/Claude 历史中 Resume/Reference。
6. 按 `Enter` 或点击 Terminal 开始输入。
7. 使用平台组合键加方向键，或点击 Back 返回导航。
8. 按 `q` 退出 Dashboard；daemon 管理的会话继续运行。

路径选择器支持直接输入文本，排序依次为前缀、子串、前向子序列匹配。`Left` 返回父目录，
`Right` 进入子目录，`Enter` 确认当前目录。Resume 页面也支持 `Left` 返回启动表单。

Runtime 一行只列出该机器实际装有的 Runtime：没装 OpenCode 的机器就不会出现 OpenCode。
Muxloom 尚未连上的机器无从判断，仍列出全部，安装提示因此不会被挡住。表单会和记住的目录
一起，默认停在你上次在这台机器上启动的那个 Runtime；若它已不在，则退回第一个可选项。

Codex Resume 来源是 `~/.codex/sessions` 和 `~/.codex/session_index.jsonl`；Claude
来源是 `~/.claude/projects`。每次都会扫描两者并用 Runtime 图标标记。相同 Runtime 原生
Resume；交叉选择会先提示类型不匹配，确认 Reference 后新建目标 Agent，并在首条 Prompt 中
给出源 History 文件。候选项优先显示 Recap，没有 Recap 时显示第一条和最后一条用户消息。
OpenCode 和 Pi 没有 Muxloom 能读的 Transcript，因此始终新建，备份只保留终端画面。
Terminal 始终新建。切换机器时，每台机器会恢复上次选中的 Agent。

<a id="zh-interface"></a>

### 界面与布局

横屏：

```text
┌──── Machines ────┬──── Agents by folder ────┬──────── Live terminal ────────┐
│ 机器与 Runtime   │ 会话、目录、Recap、状态  │ Agent CLI、Shell 或预览      │
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
通过组合键或 Back 返回。Grouped 模式下每个目录标题会整行铺一条深色底纹，不用数缩进
就能看清一个文件夹到哪里结束、下一个从哪里开始。

会话状态：

| 状态 | 含义 |
| --- | --- |
| Working | 当前可见 Agent 终端被启发式识别为正在推理或执行工具 |
| Waiting | 当前可见屏幕正在等待确认或输入 |
| Idle | 进程存活并停在普通输入提示 |
| Archived | Agent 已退出或被停止，元数据和历史仍保留 |

Working 动画**只出现在对应 agent 行**：Codex 使用青色旋转盲文 spinner（`⠋⠙⠹…`），
Claude 使用橙色 sparkle（`✻✽✶✳`），OpenCode 使用紫色菱形（`◈◇◆`），Pi 使用旋转的
`π`，均按墙钟时间推进。Folder 分组行不再闪烁，改为整行静态变色标示子会话状态——有等待
输入的子会话时变黄，有工作中的变绿；Machines 行为该机器实际装有的每个 Runtime 显示一
个静态能力图标。Working 的判定 = CLI 的中断标记（esc to interrupt）**或**实时状态行
（行首 spinner + 计时器，如 `✶ Compacting conversation… (11m 4s · ↓ 27.7k tokens)`，这
是不提供中断的阶段唯一留下的痕迹）可见 **且** PTY 最近数秒内有输出，残留在屏幕上的旧
spinner 会自动回到 Idle；压缩对话、subagent 并行阶段、无 token 计数的早期阶段都能正确
识别。Codex 另有 OSC 标题 spinner 兜底。Waiting 检测覆盖审批提
示、编号选择菜单，且自定义 `attention_patterns` 现在下沉到 daemon 按其自身刷新节奏应
用；Waiting Agent 的整个条目会变成黄色加粗。

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
| `t`（Agents） | 选择一个 Runtime，在专属临时目录启动无历史的 Temporal Chat，可选填别名 |
| `p`（Agents） | 为所选机器设置本地端口转发 |
| `Enter` | 打开 Terminal 或确认表单 |
| `x` | Live Agent 归档；Temporal Chat 直接销毁；Archived Agent 永久删除 |
| `a` | 展开或收起 Archived |
| `/` / `Ctrl-p` | 搜索全部会话历史 |
| `b` | 打开所有机器和 Agent 共用的 Talk Board；有未读时 Footer 会显示 `● N` |
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

鼠标支持点击 Focus/选择、Archive、Back 和提醒 Banner。点击在松开时生效，因此按下之后
移动再松开算滑动而不是点击。Machine 单击只选择；只有在 `[x]`
范围内双击才会启用或禁用。可以拖动所有布局分隔线；
直接拖选 Terminal 文本只是选中，松开不会复制，选区一直留在屏幕上；在 Terminal 上单击右键
复制选区，没有选区时右键则把剪贴板粘贴进会话。`Alt+拖拽`、`Alt+右键` 转发给启用 Mouse
Reporting 的程序。

<a id="zh-touch"></a>

### 触摸屏

用 Termius、Terminus 这类手机终端连上来时，手指的操作走的是和鼠标一样的 SGR 上报，
因此可以直接用：

| 手势 | 行为 |
| --- | --- |
| 在列表、Help、搜索结果上滑动 | 划过一行滚动一行 |
| 轻点 | 选中手指落点处的行、按钮或 Pane |
| 在 Terminal 或 Preview 上滑动 | 翻阅 scrollback 或文件 |
| 长按后拖动 | 选择 Terminal 或 Preview 文本 |
| 横向滑动 | 单 Pane 布局下切换到相邻 Pane，一次滑动一格 |

列表、弹窗和文件浏览器永远跟随手指；只有 Terminal Pane 和 Preview 需要在滚动和选择文本
之间二选一，由配置项 `touch` 决定：`"on"` 从一开始就按触摸屏处理，`"off"` 则所有拖拽都是
选择文本，`"auto"`（默认）自己判断。

`"auto"` 先问终端本身：Termux 只有触摸；而在 `TERM_PROGRAM` 或 `TERM` 里报出名字的终端
——iTerm2、Terminal.app、VS Code、WezTerm、Ghostty、Kitty、Alacritty、Windows Terminal
之类——都是桌面上的窗口，无论指针跳得多快都不会被当成手指。终端没给出答案时，才看指针
自己的动作：某次上报的跳变超出鼠标可能的幅度就认定是触摸屏，而之后只要出现一次不按键的
悬停上报，这个判断就在本次运行里被撤销——触摸屏上没有悬停。

<a id="zh-configuration"></a>

### 配置

默认配置是 `~/.config/muxloom/config.toml`；文件不存在时使用内置默认值。启用机器、
布局分割点、Grouped/Flat、Archive 可见性以及每台机器上次启动的目录与 Runtime 单独保存在
`~/.local/state/muxloom/state.json`。

完整 TOML 示例见英文部分的[配置](#en-configuration)。主要配置包括：

- `refresh_interval_ms`、SSH Timeout、History 大小和分段行数；
- SSH Config 路径、全局 Attention Patterns；
- Codex、Claude、OpenCode、Pi、Terminal 的 `command`、`args`、安装命令与 `sync_files`；
- 全局和每台机器独立的 `NAME=value` 环境变量；
- 每台机器独立的 `REMOTE_PORT:LOCAL_HOST:LOCAL_PORT` Reverse Tunnel；
- companion 命令和可选 Controller 本地 binary 路径；
- `touch`：Terminal 和 Preview 上的拖拽是滚动还是选择文本（`auto`/`on`/`off`）；
- `[hosts.<alias>]` 下对某台机器的 Runtime 覆盖。

`command` 是单个可执行文件名或路径，参数作为结构化数组传输。Pipe、Redirect 或复杂初始化
应写入 Wrapper。环境变量使用下面的非 JSON 格式，并默认注入 Install 和 Launch；legacy
Shell Probe 也会注入，但 daemon-native executable probe 不会：

```text
HTTP_PROXY=http://proxy:8118 HTTPS_PROXY=http://proxy:8118 TOKEN='two words'
```

选择机器按 `,` 编辑该机器的有效配置；按 `Ctrl-,` 编辑全局默认值。Args、Sync Files 和
Attention Patterns 使用 Shell Word 语法。每个 Runtime 各占一个小节；机器面板会在该机器
缺少的 Runtime 下方多出一行 `Install …`，按 `Enter` 即开始安装并关闭面板，由页脚进度条
汇报进度。

当 New 发现 Codex/Claude 不存在时，Muxloom 会先询问。无论哪条路径，都先让目标自己下载：
Controller 只解析发布元数据——版本、URL、SHA-256——由目标机器走自己的网络取回产物，并在
放到位之前用同一份摘要校验落地内容。这次拉取有明确上界（连接 8 秒，传输停滞即放弃），
所以取不到发布的机器会在几秒内快速失败并回退，而不是把安装挂住：先尝试上传 Controller
上已有的同版 binary，再由 Controller 下载并校验后推给目标，最后才执行该机器配置的用户态
安装命令（默认 Installer 需要目标直连或通过 Reverse Tunnel 访问网络）。全部失败时，错误
信息会逐条列出尝试过的方式。OpenCode 和 Pi 没有 Muxloom 能分发的 Release，直接执行各自
配置的安装命令。`sync_files` 会复制到目标用户 Home
下相同的相对路径，已有文件先备份，历史目录不会作为配置同步。

<a id="zh-mcp"></a>

### MCP

两个 binary 都能通过 Model Context Protocol（stdio 传输）把整个工作台交给 AI Agent 操作：

- **`muxloom mcp`** —— Controller 面，headless 运行。读取与 Dashboard 相同的配置和状态，
  可触达所有**已启用**的机器：列出机器与会话（含实时 working / needs_attention 状态与
  Recap）、启动与恢复会话、向会话输入、读取渲染后的屏幕与回滚、全文搜索历史、浏览与预览
  文件、归档或删除会话、执行 Shell 脚本。只有它能改动机队本身——`set_machine_enabled`
  和 `ssh_host`。TUI 不需要在运行。
- **`muxloomd mcp`** —— 同样的工具形态，但只作用于本机 daemon，适合运行在目标机器上的
  Agent。每次调用都用一条短连接访问本机 `muxloomd` socket（daemon 未运行时会自动拉起），
  因此挂着的 MCP 客户端不会推迟 daemon 升级。

重点是会话，不是 Shell。MCP 在 `initialize` 时下发的 instructions 就这么写，各个工具的
description 也反复强调：优先跟已经在那个目录里的会话对话，长任务用 `launch_session` 起，
`run_shell` 留给别的工具覆盖不到的、一次性的只读查询。

`ssh_host` 的写入只落在 `~/.ssh/config.d/muxloom.conf`（0600，带 "managed by muxloom"
头），外加在 `~/.ssh/config` 顶部补一行 `Include config.d/muxloom.conf`（缺了才补）。
如果某个别名是你自己的配置定义的，Muxloom 拒绝遮蔽它；每次写入都会把托管文件的旧内容
一并返回，所以回滚就是把那段文本写回去——或者删掉托管文件和那行 Include，SSH 配置就回到
原样。

**注册是自动的。** 每个服务于本机自己状态目录的 `muxloomd`——本机的和各远端的
companion——都会以 `muxloom` 为名往该用户的 `~/.claude.json` 和 `~/.codex/config.toml`
写一条 MCP 条目，所以跑在某台机器上的 agent 无需任何配置就能看到并驱动这台机器上的会话。
每台机器只有这一条，指向那台机器上装了的最好那一面：daemon 旁边就装着 controller 时指向
`muxloom mcp`（够得着整个机队），只装了 companion 的机器则指向 `muxloomd mcp`。你推过
companion 的远端拿到 daemon 的条目，你用来驱动机队的这台拿到 controller 的，绝不会两条
并存。而一个被交付了状态目录才起来的 daemon——测试用的、临时的、你正在调试的第二个——
不属于这台机器，什么都不认领，于是你手边的 agent 仍然指着那个真正装着你会话的 daemon。
确实想让这类 daemon 接管条目，就设 `MUXLOOM_MCP_REGISTER=1`。只写这一条目，且只在缺失或
指向过期路径时才写，解析不了的文件原样保留。同一次启动还会给 Claude Code 留一份
`~/.claude/skills/muxloom/SKILL.md`，讲清楚这套机队怎么协作；文件带版本戳，只在戳是
Muxloom 的且已过期时才重写，所以你一旦改过它，它就归你了。Codex 没有 skill 机制，靠 MCP
`instructions` 拿到精简版。在 daemon 环境里设置 `MUXLOOM_MCP_REGISTER=0` 可以整体关闭，
`MUXLOOM_SKILL=0` 则只关 skill、保留 MCP 条目。

手工注册跨机器的 Controller 面：

```bash
claude mcp add muxloom -- muxloom mcp
```

Codex（`~/.codex/config.toml`）：

```toml
[mcp_servers.muxloom]
command = "muxloom"
args = ["mcp"]
```

Agent 驱动 Agent 的典型流程：`list_sessions`（或 `launch_session`）→ `send_input` 带
`submit: true` 提交 Prompt → 轮询 `list_sessions` 等 `working` 结束（`needs_attention`
会带上命中的审批提示）→ `read_screen` 读取结果。屏幕以纯文本返回：颜色、光标移动和标题
序列被剥掉，而转义序列跳到的列会补成空格，因此菜单读起来仍然是菜单。

通过 MCP 启动的会话就是普通托管会话：会出现在 Dashboard 中、在 MCP 客户端退出后继续运
行，也要像其他会话一样归档或删除。未启用的机器不会被触碰——指向它的调用会被拒绝。

工具面本身与传输无关（`src/control.rs`）；MCP stdio 是它的第一个适配器，同一接缝将来
可以接 TCP 或串口适配器，让硬件状态面板读取 Agent 状态。

> [!WARNING]
> `run_shell` 与 `send_input` 允许连接的 MCP 客户端在启用机器上以你的用户身份执行任意
> 命令，请据此决定向哪些客户端开放。

<a id="zh-collaboration"></a>

### Agent 协作层

上面那组工具让一个 Agent 驱动整个机队；这一组让一群 Agent 一起干活：一块所有人共读共写的
公告板、可跨机直达另一个会话的私信，以及对所有人说过的话的检索。这里没有主从——别的 Agent
发来的消息是请求不是命令，人在 Dashboard 里发的帖和 Agent 发的帖也完全同构。

**Talk Board。** `talk_post` 发言，`talk_read` 读取。每条消息都有 scope：

| Scope | 谁看得到 | 用途 |
| --- | --- | --- |
| `path`（默认） | 同一台机器同一个目录里的所有人 | 项目频道：我在改什么、我发现了什么 |
| `machine` | 这台机器上的所有人 | 主机级消息：某个服务挂了、盘满了 |
| `global` | 所有机器上的所有人 | 真正需要传遍全网的事 |
| `direct` | 单个会话 | `message_agent` 的回复，以及它的投递记录 |

默认读到的是"摆在你面前的"——本机、本目录、global，以及发给你的私信；
`include_machines` / `include_paths` 可以放宽到指定的机器和目录，或者 `"all"` 搜遍全部。
`kind: "note"` 把公告板当记忆用：Agent 的上下文随对话结束，note 不会。`since_cursor` 只返回
新增内容，`wait_seconds` 则把调用挂住直到有人说话，不用轮询。

scope 只决定消息**是给谁的**。所有消息都会复制到每台机器，所以公告板在哪台机器上读起来
都一样，某台机器连不上时也照样能读。存储是每台机器一份 append-only 日志（`<state>/talk/`），
按版本向量合并，同步由 Controller 驱动——没有 Dashboard 连着时，机器之间会暂停交换消息，
直到有一个连上来。

**私信。** `message_agent { machine, session_id, text }` 会把消息连同一段信封打进对方会话的
输入框，信封写明发件人、所在机器与目录，以及怎么回信，因此对面的 Agent 知道这是同事在说话，
不是它的用户在说话。默认会等对方忙完再投（`deliver: "when_idle"` 一直等，`"now"` 直接打断）。
每条私信同时落在公告板上，所以说了什么、有没有送到都可追溯，回信走
`talk_read { scope: "direct" }`。

**等待。** `wait_for` 会阻塞到会话空闲、需要人介入、屏幕出现某段文本、安静下来或退出为止——
超时是正常返回，不是失败。`trigger` 则把这份守望交给 daemon，用于没有任何对话在场的时候；
触发器能跨 daemon 升级存活，会随会话消失而消失。

**回忆。** `search_conversations` 检索所有已启用机器的历史对话，`read_conversation` 按消息
序号分页读取其中一段，不会把整段对话灌进上下文。两者读的都是备份快照，进行中的对话可能
落后一个备份周期。

**可达性。** `muxloom mcp` 直接连所有已启用机器；远端的 `muxloomd mcp` 自己没有机队，
它的跨机调用由挂着的 Controller 代跑，而且只代跑这些：对话检索与回忆、列机器与会话、
`message_agent`、公告板同步。Shell、机器启停和 SSH 改写永远不代跑。没有 Dashboard 连着时，
这些调用立刻失败并说明原因。

**在 Dashboard 里看。** 按 `b` 打开公告板，版式就是 BBS：顶部 scope 标签页，一行一条，
新的在下面，`/` 过滤，`Enter` 展开，`p` 以你自己的身份发帖，`r` 回复。有未读时 Footer 上
挂着 `● N`。

**Moderator。** Moderator 是你用来代替"对着整个机队说话"的那个 Agent：活交给它，由它判断该谁
来做、用 `message_agent` 分下去、跟进、然后回来告诉你结果。机器列表最上面钉着一行
**Moderators**，在那行按 `n` 就能起一个。

表单问三件事：运行时（Codex 或 Claude，muxloom 只把控制面注册进这两个）、名字，以及这个
Moderator 该看着哪些机器、哪些 Agent。初始全部勾上，读作"整个机队，包括之后才出现的"，
取消勾选就是收窄。不用选目录：muxloom 会在 `<state>/projects/<名字>/` 下给它开一个专属
文件夹，并把 briefing 同时写成 `CLAUDE.md` 和 `AGENTS.md`，所以 Agent 一起来就读到，不需要
往它嘴里塞一段开场白。

这个范围是 briefing，不是沙箱。MCP 面不会因此少答应什么——Moderator 和这里任何一个 Agent
一样，够得着你启用的每一台机器——briefing 里也把这句话原样写着，并要求它越界之前先问你。
真想拦住它，用要保护的那台机器上的 `[mcp] denied_tools`。

Moderator 列在自己那一行下面，不算在"本机"里；它的文件夹也永远不会变成本机下次启动的默认
目录。要停掉它，和停掉任何 Agent 一样按 `x`。

**收紧权限。** `[mcp] denied_tools` 让某个工具从工具列表里消失、按名字调用也被拒；
`read_only = true` 一次禁掉所有会改变状态的工具。每台机器各自说了算，所以远端上跑的 Agent
能做什么，由那台机器自己的 `config.toml` 决定。只禁 `message_agent` 的话，公告板照样可读
可写，但 Agent 之间不能互相打断。

<a id="zh-sessions"></a>

### 会话、历史与提醒

`muxloomd` 直接持有 PTY 和子进程，Dashboard 与 SSH Bridge 只是订阅者。重连后按 Session
ID 恢复订阅。daemon 在目标端追加保存 ANSI History；旧页面按需分段读取并缓存在本机，
有效大块使用 LZ4。Offset 会限制在真实历史内，不会滚到不存在的空白区域。

历史渲染保留基础色、256 色、Truecolor 前景/背景以及 Bold、Dim、Italic、Underline、
Reverse 和 Crossed-out 属性。

Recap 先取最后一个 `※ recap:`（也支持全角冒号）；否则取最后一条能识别的 Agent
Assistant 行并排除工具/状态行；仍无法识别时，回退到最后一条非界面文本。结果会归一化
控制字符和空白，并限制在 180 字符。按 `/` 或 `Ctrl-p` 搜索 Live/Archived、本地/远端
全部会话，排序优先级是 Label/名称/路径、当前 Recap 与 Recap History、其他 History。

Agent 退出或第一次按 `x` 后进入 Archived，仍可查看和搜索。打开 Archived 会按
原机器、Runtime 和目录尝试 Resume 最新历史。确认框默认勾选在新 Agent 成功启动后移除旧
Archived 条目，按 `Space` 可选择保留，该选择会持久记忆；启动或清理失败时旧归档保持不变。
再次按 `x` 才永久删除 daemon 元数据与历史。
普通 Terminal 不归档，Shell 退出或按 `x` 后直接清理。

在 Agents 面板按 `t` 会先在该机器已装的 Runtime 中选一个，再启动 `Temporal Chat`。它不会寄生在当前
选中的项目目录里，而是由 muxloomd 在 `<muxloomd 状态目录>/scratch/<会话 ID>` 下新建一个专属临时目录，
随会话一起删除，所以草稿 Agent 不会在任何仓库里留下痕迹；同一个表单里可以填一个别名区分多个临时
会话，留空则统一叫 Temporal Chat。该会话不写 Muxloom ANSI History，也不进入搜索或备份；
Codex 还会用单次配置关闭 Transcript 持久化。按 `x` 会直接停止并删除，不进入 Archived。
临时会话永远排在会话列表最顶端、所有文件夹之上——几秒前刚开的草稿窗口就是你要找的那个。

在 Agents by folder 按 `p` 可把所选机器上的服务转发到 Controller 的 Loopback。填写远端
Host/Port 与本地 Port（`0` 表示自动分配），之后访问 `127.0.0.1:LOCAL_PORT`。Linux companion
会原生探测非特权监听端口；所有平台也会从 Agent 当前终端中可见的 Loopback URL 提取候选。
探测不可用时仍可手动填写。TCP 流量复用该机器已有的持久 Bridge；选中活动转发按 `d` 停止，
不会停止远端服务或 Agent。本地 Listener 只在当前 Muxloom Controller 进程期间存在。

提醒只检查当前屏幕底部物理行。Attached 和 legacy-inspected Session 会组合内置审批布局与
每机器 Pattern；每机器的 `attention_patterns` 现在也会下发给 daemon，由它按自身刷新节奏
应用到后台 snapshot。新提醒会把整个 Agent 条目显示为
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
| 在选中的 Preview 文本上单击右键 | 复制选区；没有选区时右键仍然返回父目录 |
| 方向键、`PageUp` / `PageDown` | 对打开内容翻页，并停在开头或末尾 |
| `g` / `G`、`Home` / `End` | 跳到 Preview 开头或末尾；停在末尾时会随文件增长自动跟随 |
| `c` | 复制目标机器上的完整路径 |
| 在 Preview 上拖拽 | 选中 Preview 文本；右键复制 |
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

Bootstrap 由 Rust binary 自己计算 SHA-256 Fingerprint。缺失或过期的 companion，只要发布
提供的正是我们本来要推的那份字节，就由目标自己去取——Controller 把 URL 和摘要交给它，
它校验落地内容——否则仍通过同一条 SSH stdin 原子安装。如果 daemon provisioning 或 launch 失败且目标有 tmux，可以进入明确
标记且必须确认的兼容回退。

每个会话由一个极小的 **keeper** 进程持有：只负责 PTY、子进程和原始历史追加，协议永久
冻结，因此它本身几乎不需要更新。daemon 只是 keeper 的当前客户端——负责屏幕、状态、
搜索与元数据。daemon 升级不再等待空闲：换代时会话由各自的 keeper 原地带过去，新
daemon 连上 keeper socket 即收养（同一进程、同一转录），daemon 崩溃也不再杀死会话。
运行中的 daemon 落后于当前构建时，footer 右下角会出现 `⟳` 标记，Controller 会在该机
终端未 attach 时自动重连完成升级。pre-keeper 旧会话会无限期推迟接管；在 Machines 面板
选中该机按 `,` 打开设置面板，其中会显示该机 `muxloomd` 的运行版本，并提供 **Force update**
动作一次性强制更新——先弹窗列出将被打断的 working agent 与将被终结的终端（终端无法恢复），
确认后归档、完成接管、再从各 agent 自身的转录自动 resume。History
和 Metadata 始终保留在状态目录。

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

- Machine Offline：机器行稳定显示红色 `!`，后台重试不再反复闪 Connecting；按 `r` 手动重试
  才显示进度。排查先执行 `ssh -T -o BatchMode=yes <alias> true`，再看 Bootstrap 错误；
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

- Codex 与 Claude 私有 History 格式不同，OpenCode 和 Pi 则没有 Muxloom 能读的格式；跨
  Runtime Reference 会新建会话并在首条 Prompt 中引用源 History 文件，不会伪装成原生
  Resume 或转换私有格式；
- Windows 暂时只能作为远程 Controller；
- Audio Playback、Video Seek 和音量控制暂未实现；
- Resume 依赖 Codex/Claude 当前的本地元数据格式，OpenCode 和 Pi 只能新建；
- Attention 是启发式检测，每台机器的 Pattern 应尽量具体；
- 启用机器意味着允许周期性 BatchMode SSH 和 companion 管理；
- 目标 History、Debug Snippet 和搜索结果都可能包含敏感内容；
- 连接 `muxloom mcp` / `muxloomd mcp` 的 MCP 客户端可以读取历史、向会话输入并以你的用户
  身份在启用机器上执行 Shell 脚本，`[mcp] denied_tools` 与 `read_only` 可以按机器收窄；
- `message_agent` 是往别的 Agent 输入框里打字，等同于替它按回车；Talk Board 上的消息无论
  scope 都会完整复制到每台启用机器，因此应当把公告板视为全机队可见，不要在上面放密钥；
- Muxloom 默认不添加跳过 Agent 权限检查的参数，用户配置的 Runtime Args 仍具有对应风险。

---

## License

Muxloom 依据 [GNU General Public License v3.0 only](./LICENSE) 分发。
