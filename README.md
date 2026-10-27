<p align="center"><img src="assets/icon.png" width="72" height="72" alt="AI 工具看板图标"></p>

# AI 工具看板 · AI Tool Dashboard

在原生桌面悬浮窗口中查看 AI Agent 的**剩余额度**和**本机正在执行的任务**。

采用 Rust + Slint，共享界面与状态模型，各工具使用独立适配器。以低常驻开销为目标，不依赖常驻 WebView、Node.js 或 Python。

> 当前为早期原型。Windows x64 已进行本机验证；macOS、Linux 有对应实现，构建、托盘和窗口行为仍待各平台实测。暂未提供安装包或 GitHub Release。

## 功能

- **剩余额度**：数字和进度条统一显示 `100% − 已用比例`，展示来源实际返回的额度窗口与重置时间。
- **本机任务观察**：核对明确的任务开始/结束事件与所属运行时，按会话去重；ZCode 内部子 Agent 不重复计数。
- **极简悬浮条**：两个工具时为 292×62，无标题栏，可拖动；Windows 极简模式隐藏任务栏按钮，保留悬浮条和托盘。
- **完整看板**：深浅主题、置顶、圆润卡片与胶囊按钮；Windows 11 请求系统原生窗口圆角。
- **系统托盘**：显示看板、收进后台、打开设置与真正退出；完整模式或打开设置时恢复任务栏按钮。
- **后台运行和登录启动**：可在设置中开关，记住主题、模式、置顶与位置。
- **明确的数据状态**：区分缺失、读取失败与过期，不将未知数据显示成 0。

## 当前接入范围

| 工具 | 额度 | 进行中任务 |
|---|---|---|
| Codex | 当前 ChatGPT 登录账号的订阅额度，支持来源实际返回的多个窗口 | 本机可核实的任务数量下限 |
| ZCode | 当前 BigModel 个人 Coding Plan 的五小时/周额度 | Windows ZCode 3.16.3 本地内置运行时的顶层会话数量下限 |

Z.ai、团队及 Start Plan 额度、远端执行后端、其他设备和等待审批/输入数量尚未接入。

### 如何理解任务数

- `≥2`：至少核实了 2 个本机进行中会话，有限日志不能保证完整覆盖。
- ZCode 显示 `0`：来源健康、日志已追平，已观测任务明确结束；只表示已观测范围内空闲。
- `—`：证据不足、来源不可用或记录过期，不能据此认定没有任务。

进程数、打开的聊天数、最近修改的文件数均不直接作为任务数。额度读取成功也不代表客户端在线或有任务正在执行。

## 构建与运行

先在对应客户端中正常登录，本程序读取现有账号状态，不提供登录或认证修复功能。

### 已有 Rust 工具链

需要 Rust 1.92+、对应平台的 C/C++ 编译工具及 Slint 平台依赖。仓库包含应用的 `Cargo.lock`，请使用 `--locked` 构建。

```sh
git clone https://github.com/QQ87146069/AI-Tool-Dashboard.git
cd AI-Tool-Dashboard
cargo build --release --locked
```

Windows：

```powershell
.\target\release\ai-tool-dashboard.exe
```

macOS / Linux：

```sh
./target/release/ai-tool-dashboard
```

Linux 需要图形桌面及 [Slint Winit 平台依赖](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/)，托盘还需要支持 StatusNotifierItem 的桌面宿主或扩展。Wayland 的定位、拖动与置顶取决于合成器。macOS 使用菜单栏图标。这些平台尚未完成实机验收。

### Windows 项目内便携工具链

引导脚本会下载工具到 `.tools`，不修改系统 PATH；额外开发磁盘空间不属于程序运行依赖。

```powershell
# 首次准备 Rust GNU / MinGW 工具
.\scripts\bootstrap-windows.ps1
# 后续在新的 PowerShell 中加载项目内工具链
. .\scripts\dev-env.ps1
cargo build --release --locked
.\target\release\ai-tool-dashboard.exe
```

本机已验证 Rust 1.99.1 GNU / GCC 16.2.0。默认使用软件渲染；`.tools`、`target` 和本地分发产物不提交到仓库。

## 使用方式

| 操作 | 行为 |
|---|---|
| “简” / “详” | 切换极简悬浮条与完整看板 |
| 拖动标题或极简工具行 | 移动窗口 |
| “顶” | 开关置顶 |
| “浅” / “深” | 切换主题 |
| “−” | 最小化到任务栏 |
| “×” | 按后台设置收进托盘或退出 |
| 完整模式的“设置” | 管理开机启动与关闭后后台运行 |
| 托盘菜单“退出程序” | 停止后台刷新并退出 |

**开机启动默认关闭，关闭后后台运行默认开启。** 登录启动使用 `--background`，只显示托盘，不自动弹出悬浮窗；可从托盘“显示看板”恢复。Windows 图标也可能位于右下角“^”折叠区。

程序移动位置后，请关闭再开启开机启动，以更新注册路径；删除便携程序前请先关闭此设置。启动项范围为当前用户：Windows Run、macOS LaunchAgent、Linux XDG autostart。

### 命令行和环境变量

```sh
# 仅启动托盘
ai-tool-dashboard --background
# 指定本程序偏好文件，不影响客户端配置
ai-tool-dashboard --state /path/to/window.json
```

`--state` 优先于 `AI_DASHBOARD_STATE`。未指定时使用系统应用配置目录。

| 变量 | 用途 |
|---|---|
| `CODEX_HOME` | Codex 数据根目录，默认用户目录下 `.codex` |
| `ZCODE_HOME` | ZCode 数据根目录，默认用户目录下 `.zcode` |
| `AI_DASHBOARD_STATE` | 本程序偏好文件路径 |
| `AI_DASHBOARD_RENDERER` | 默认 `software`；`femtovg` 需要 GPU 特性构建 |
| `ZCODE_CREDENTIAL_SECRET` | 仅在 ZCode 使用该凭据加密配置时提供相同环境；不自动猜测或重置 |

自定义来源、代理与解密环境变量需在登录启动环境中同样可用，不会写入启动项。

## 数据与隐私

- 客户端文件、数据库和会话状态保持只读；不自动登录、刷新认证或写回凭据，不启动额外客户端 app-server。
- 只在内存中使用当前账号必要的 access token；ZCode 仅解密当前支持账号相关的必要字段。
- 额度请求限定固定 HTTPS GET，启用证书校验、禁止重定向，并限制请求时间与响应大小。
- 任务观察提取生命周期、时间和归属元数据，不解析、保存或展示聊天、模型报文及工具输出正文。
- 额度正常查询最短间隔 60 秒，失败后指数退避；任务状态约 3 秒检查，计数过期后隐藏。
- Windows 使用系统静态代理；其他平台支持 ureq 的代理环境变量。PAC 与其他平台网络行为待进一步验证。

偏好文件同目录的 `startup.log` 最多 32 KiB，仅记录本程序启动方式、事件循环就绪、退出或初始化错误，不记录参数、环境变量、账号与会话数据。写入失败不阻止启动。

仓库不包含开发者的认证文件、本机运行日志、真实额度截图或客户端数据。

## 已知限制

- 仍为原型，客户端升级可能改变接口、日志和数据库结构；兼容性失效时显示未知或失败。
- 任务数是本机可核实下限，不能替代全局权威统计；长时间没有同回合事件的任务可能失去确认。
- Windows 曾出现启用 Run 项但登录后未发现程序运行的情况，根因仍待确认；相同命令手动启动与托盘运行已验证，新增日志用于排查。尚不能保证所有登录环境自动启动成功。
- macOS/Linux、系统登录、休眠恢复与长时间故障场景仍需更多实机验证。
- 尚无安装器、自动更新或动态插件市场。

## 开发

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

可选 GPU 构建：`cargo build --release --locked --features gpu`，随后设置 `AI_DASHBOARD_RENDERER=femtovg`。默认仅启用软件渲染。

`verification` 特性提供开发验收回调与快照，不用于正常分发。Windows 托盘/任务栏脚本为 `scripts/verify-tray-windows.ps1`，实际任务栏 UI Automation 检查需要 Windows PowerShell 5.1。`-TestStartup` 仅允许在本程序启动项不存在时使用，避免覆盖已有设置。

| 目录 / 文件 | 内容 |
|---|---|
| `src/model.rs` | 共享状态与新鲜度 |
| `src/adapters/` | 独立工具适配器、额度和任务观察 |
| `src/main.rs` | 窗口、托盘与后台调度 |
| `src/startup.rs` / `src/startup_log.rs` | 登录启动与有界日志 |
| `ui/dashboard.slint` | 共享完整/极简界面 |
| `assets/` | 图标源文件与平台图标 |
| `scripts/` | 构建辅助与本机验收工具 |
| `examples/probe.rs` | 显式运行的真实来源单次探测；单元测试不会自动运行它 |

新增工具时添加独立适配器并注册即可，无需复制整套界面。

## 许可

本项目暂未声明独立的项目许可证。第三方依赖遵循其各自许可，Slint 的授权选项见 [官方说明](https://slint.dev/license/)。
