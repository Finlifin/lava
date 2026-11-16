# Lava 系统架构深度参考 *lava-architecture*

本文档深入剖析 Lava 编程环境的分层架构、单 Nessa 虚拟机包图合成机制、内容寻址 `.nsbc` 字节码缓存、GTK4 无锁双线程渲染管线、宿主原生协议（Host Protocol）以及基于代数效应与多提示符定界延续（Delimited Continuations）的交互式事件调度模型。返回手册总目录请参阅 `[index#top]`。

---

## 八大核心架构原则 *architecture-principles*

Lava 从立项之初便拒绝“庞大的 C/Rust 硬编码编辑器 + 受限胶水脚本层 + 跨进程 JSON-RPC 插件宿主”的传统架构，确立了八条严格正交的系统不变量：

### 原则一：单一共享 Nessa 虚拟机 *single-vm*

整个 Lava 进程只实例化 **唯一一个 Nessa 虚拟机（`lava-vm`）**。4 个核心库包（`packages/*`）、40 个内置功能扩展（`extensions/*`）、用户通过 `--ext-dir` 挂载的第三方包，以及用户的个人配置脚本（`~/.config/lava/init.ns`）在启动期被组合进同一张多包编译依赖图（`LavaWorkspace::compose_boot`），编译为单个 finalized `TypePool` 与字节码归档，并在同一 GC 堆与 `SystemKernel` 实例中运行：

- **零序列化开销**：`ext_telescope`、`ext_gitsigns` 或用户的 `init.ns` 读取 `EditorWorld` 中的 `Buffer` 行列表时，传递的是直接的 Nessa 引用值，无需 JSON 编解码或跨进程管道拷贝。
- **跨包强类型互操作**：由于单次全局类型池收敛（`TypePool` finalization），不同扩展包对同一结构体（如 `Pos`、`BufferId`、`KeyStroke`、`FloatingWindow`）共享完全一致的稳定 `TypeId`，模式匹配（`payload matches _: BufferId`）与向下转型（`.as(EditorWorld)`）具备零歧义的运行时安全性。

### 原则二：扁平化 Rust/GTK4 宿主底座 *flat-rust-core*

Rust 层严格限制为**硬件与操作系统抽象层**，绝不包含“当前处于 Normal 还是 Insert 模式”“哪些窗口分屏怎样排列”或“状态栏显示什么文字”等任何编辑器业务概念。Rust 工作区仅由 6 个职责单一的 Crate 构成（详见下文 `[Info: *rust-crates-overview*]`）。

### 原则三：Nessa 全面组织应用层 *nessa-app-layer*

所有上层概念——文档缓冲区、分叉撤销树、寄存器、分屏二叉树、标签页、8 套主题调色板、多语言词法着色器、Minibuffer 控制器以及顶层事件循环——全部由 `packages/*` 与 `extensions/*` 下的 Nessa 代码实现。修改任何 UI 呈现或编辑语义都只需编辑 `.ns` 源码。

### 原则四：Extension 为唯一一等公民单元 *first-class-extensions*

在 `SystemKernel` 中，`ext_document`、`ext_window`、`ext_render`、`ext_vim` 与 `ext_harpoon`、`ext_magit` 具有完全相同的身份：它们都是注册在内核中的 `ExtensionDescriptor`，声明各自的 `ExtId`、显式拓扑依赖列表（`deps`）、激活回调（`activate`）与停用回调（`deactivate`）。内核保证按拓扑顺序激活，并在停用某扩展时自动校验反向依赖、自动卸载其名下注册的所有服务（Services）、命令（Commands）、钩子（Hooks）与键位绑定（KeyBindings）。

### 原则五：用户脚本与内置扩展同构统一 *unified-script-world*

`init.ns` 不是受限的配置文件，而是由组合器实时合成的一等 Nessa 库包 `user.local/user_init`。它自动获得工作区所有非冲突库包的 `@pkg` 导入权限，可以定义自己的数据类型、在 `pub fn configure(kernel: SystemKernel)` 中调用 `kernel.register_extension(...)` 注册运行期扩展，甚至通过 `--ext-dir` 覆盖同名内置扩展（详见 `[extension-development#top]`）。

### 原则六：代数效应与能力处理器驱动横切服务 *effect-services*

扩展内部从不直接耦合平台全局单例，而是通过 `lava_core` 声明的代数效应（`log_event`、`status_emit`、`clipboard_get`、`clipboard_set`、`ask_prompt`、`spawn_bg_job`、`sleep_ticks`）表达对外部能力的需求。在生产会话中，`ext_app` 安装 `AppHostCap` 将效应桥接至 GTK4 与系统剪贴板；在单元测试中，测试套件直接安装内存态 `LogCollectorCap`、`StatusCollectorCap` 与纯内存剪贴板，无需启动任何图形服务器即可 100% 确定性验证所有插件逻辑（详见 `[nessa-for-lava#top]`）。

### 原则七：交互模型完全由扩展定义 *extension-defined-interaction*

Lava 内核本身既不是 Vim 也不是 Emacs。模态编辑状态机由 `ext_vim` 提供，Emacs 无模态与前缀键体系由 `ext_emacs` 提供，特殊缓冲区局部键位由 `mode:<major_mode>` 键位表提供。用户随时可通过 `<C-z>`（`toggle-evil-mode`）或关闭对应扩展来重塑交互体验（详见 `[vim-and-emacs-keybindings#top]`）。

### 原则八：UI 线程与 VM 线程彻底无锁解耦 *decoupled-threads*

GTK4 主事件循环线程与 Nessa VM 工作线程严格隔离，通过单槽原子交换器 `FrameSlot` 与跨线程事件队列通信，确保 UI 缩放、重绘与输入采集永不被复杂计算卡顿。

---

## 分层架构与 Rust 宿主六大 Crate *rust-crates-overview*

```text
┌───────────────────────────────────────────────────────────────────────────┐
│                Nessa 应用与扩展层 (Single Shared Nessa VM)                │
│  user_init (init.ns)  │  40 Builtin Extensions (ext_app / ext_vim / ...)  │
│  4 Core Nessa Packages: @lava_core │ @lava_sys │ @lava_accel │ @lava_term │
└───────────────────────────────────┬───────────────────────────────────────┘
                                    │ std.runtime.host_call(...)
┌───────────────────────────────────▼───────────────────────────────────────┐
│                      lava-vm (Composer + LavaHostHandler)                 │
│   包发现与 lava_boot 合成 │ .nsbc 内容寻址缓存 │ SuspendRetry 事件挂起唤醒 │
├──────────────────┬────────────────┴───────────────┬───────────────────────┤
│    lava-gtk      │          lava-accel            │       lava-term       │
│ GTK4 + Cairo 2D  │ 并行模糊匹配 / Grep / Myers    │ Linux POSIX openpty   │
│ 窗口与输入控制器 │ Diff / Git / Info 文档全文索引 │ VT100/ANSI/TrueColor  │
├──────────────────┴────────────────────────────────┴───────────────────────┤
│              lava-proto (DrawCmd / RenderFrame / UiEvent / FrameSlot)     │
└───────────────────────────────────────────────────────────────────────────┘
```

各 Rust Crate 的精确边界如下：

1. **`crates/lava-proto`**：纯数据结构与并发同步原语库（不依赖 GTK4 或 Nessa VM）。定义平台无关的 7 种 2D 绘制指令 `DrawCmd`、完整帧快照 `RenderFrame`、9 种原始平台事件 `UiEvent`，以及基于 `AtomicU64` 世代号与 `Mutex<Arc<RenderFrame>>` 的单槽无锁帧交换器 `FrameSlot`。
2. **`crates/lava-gtk`**：GTK4 图形前端与离线渲染器。在 GTK4 主线程创建 `DrawingArea`、键盘/鼠标/滚轮事件控制器与剪贴板桥接；当收到 VM 线程的帧唤醒信号时，调用 `FrameSlot::latest()` 取出最新 `Arc<RenderFrame>`，使用 **Pango + Cairo** 精确绘制裁剪矩形、背景色块、等宽字体文本段（`TextRun`）与四种光标形状（`Block`、`Beam`、`Underline`、`HollowBlock`）。同时提供 `render_frame_to_png_file` 支持无头 PNG 渲染导出。
3. **`crates/lava-vm`**：单 VM 引导组合器（`composer.rs`）与宿主外部函数处理器（`host.rs`）。负责扫描工作区包、合成 `user_init` 与 `lava_boot` 根可执行包、读写 `.nsbc` 字节码缓存，并实现 `HostCallHandler` trait 处理全部系统、UI、加速与 PTY 宿主调用。
4. **`crates/lava-accel`**：无状态/并发安全的 Rust 原生算法加速引擎。利用多核工作线程提供工作区文件遍历（自动尊重 `.gitignore` 并跳过 `.git` 与 `target`）、子序列/词边界模糊打分（`fuzzy_rank`）、并行子串/大小写感知 `workspace_grep`、原子多文件 `workspace_replace`、Myers 行级差异算法（`diff_lines`）、Git 状态/HEAD/行级 Blame 解析、多语言符号大纲提取、TODO 扫描，以及 Markdown/Nessa 文档分节索引与五域打分引擎（`index_docs` / `search_docs` / `find_symbol_defs`）。
5. **`crates/lava-term`**：原生 Linux POSIX 伪终端与虚拟屏幕模拟器。使用 `openpty`、`fork`、`setsid`、`TIOCSCTTY` 与 `TIOCSWINSZ` 管理真实子 Shell 进程，后台读取线程将字节流实时喂入内置 **VT100 / xterm / ANSI 16/256/24-bit TrueColor** 解析器 `VtScreen`（支持主屏幕与 `?1049h` 备用屏幕、滚动区间 `DECSTBM`、OSC 0/2 标题与 OSC 7 工作目录跟踪），并通过定时器事件主动唤醒 VM 刷新画面（详见 `[terminal-and-pty#top]`）。
6. **`crates/lava-cli`**：轻量命令行入口，解析启动标志并协调 `LavaWorkspace`、`LavaHostHandler` 与 `lava_gtk` 会话生命周期。

---

## 单 VM 引导合成与 `.nsbc` 内容寻址字节码缓存 *nsbc-cache*

为了同时兼顾“40+ 个 Nessa 包在单 VM 内静态强类型链接”与“日常打开文件毫秒级瞬时启动”，`crates/lava-vm/src/composer.rs` 实现了一套精巧的两阶段引导合成与字节码缓存架构：

### 包发现与 `lava_boot` 动态合成 *boot-package-synthesis*

调用 `LavaWorkspace::discover(root)` 与 `compose_boot(options)` 时，组合器按以下优先级组装完整单 VM 包图：

1. **核心包（`PackageOrigin::CorePackage`）**：扫描 `packages/*`，加载 `lava.org/lava_sys`、`lava.org/lava_core`、`lava.org/lava_accel`、`lava.org/lava_term`。
2. **内置扩展（`PackageOrigin::BuiltinExtension`）**：扫描 `extensions/*` 下的全部内置插件包。
3. **用户扩展目录（`PackageOrigin::UserExtension`）**：按命令行 `--ext-dir` 传入顺序扫描用户包；若存在相同限定名（`<domain>/<name>`），用户包平滑覆盖内置包。
4. **用户初始化包合成（`user.local/user_init`）**：若存在 `~/.config/lava/init.ns`（或 `-u <path>` 指定脚本且未开启 `-q`），组合器自动为其生成内存态 `package.toml`，将工作区内所有非冲突库包注入 `[dependencies]`，使 `init.ns` 可直接书写 `use @lava_core.*`、`use @ext_document.*` 等导入语句。
5. **根引导包合成（`lava.org/lava_boot`）**：组合器自动扫描所有内置与用户扩展入口源码中的顶层 `pub fn register_<stem>_extension(kernel: SystemKernel)` 函数，动态生成 `lava.org/lava_boot`（`type = "exe"`）的 `main.ns`：
   - 调用入口包（默认 `lava.org/ext_app`）的 `boot_kernel()` 构造 `SystemKernel` 并注册基础扩展；
   - 依次调用所有自动发现插件的 `register_*_extension(kernel)`；
   - 若用户 `init.ns` 定义了 `pub fn configure(kernel: SystemKernel)`，调用 `user_init.configure(kernel)`；
   - 调用 `kernel.activate_all()!` 完成全图拓扑激活；
   - 进入 `ext_app.run_session(kernel, startup_files, headless_keys, headless = headless_mode)` 主事件循环。
   - 同时注入内部模块 `__lava_host_abi_types`，预先驻留 `std.runtime.host_call` 可能返回的所有 `(bool, T)` 元组与 `?T` 可选类型，确保多包 `TypePool` 固化后无需任何运行期动态结构类型创建。

### 128-bit 内容哈希与零重编译动态参数注入 *nsbc-fingerprint*

`compile_composed_program_cached` 使用确定性 128-bit FNV-1a 双通道哈希器（`ContentHasher128`）计算缓存键 `<32位十六进制>.nsbc`，哈希输入涵盖：

- 缓存协议魔数 `"lava-nsbc-cache-v1"`、`lava-vm` Crate 版本号与底层 `nsbc::VERSION` 字节码归档格式版本；
- 经确定性排序后的每一个参与编译包（含合成后的 `user_init` 与静态引导模板 `lava_boot`）的 `package.toml`、入口源码与全部子模块源码。

**关键设计亮点**：组合器在计算缓存哈希时，会将易变的命令行运行期参数（打开的文件列表 `startup_files`、注入按键 `--keys` 与 `--headless` 布尔值）归一化为空模板！当 VM 启动缓存的 `.nsbc` 归档时，`lava_boot` 首行立即调用宿主原语 `runtime.host_call("sys_boot_config", [])`，从 `HostConfig` 实时读取本次启动的真实文件路径与按键序列。因此：

- 只要 `.ns` 源码与 `init.ns` 未发生变更，无论您执行 `lava a.rs`、`lava b.ns c.md` 还是运行 `--headless --keys ...`，**100% 命中同一份 `.nsbc` 字节码归档，编译耗时为 0 ms**！
- 缓存目录解析顺序：优先读取环境变量 `LAVA_NSBC_CACHE_DIR`（设为 `0`、`false`、`off` 或 `none` 可关闭缓存）；若未设置且工作区存在 `target/` 目录，则存放在 `<workspace>/target/lava-nsbc-cache/`；否则存放在系统临时目录 `$TMPDIR/lava-nsbc-cache/`。缓存采用“进程 PID 唯一临时文件写入 + `fs::rename` 原子替换”，多实例并发启动绝对安全。

---

## 无锁双线程渲染管线（`FrameSlot` & `DrawCmd`） *render-pipeline*

Lava 将一帧画面的生命周期严格划分为 **Nessa 逻辑合成阶段** 与 **GTK4 PangoCairo 光栅化阶段**：

```text
[Nessa VM 线程]                                      [GTK4 UI 主线程]
1. RawUiEvent 唤醒 (Key / Mouse / Resize / Tick)
2. ext_app 路由事件 -> 更新 EditorWorld & Panes
3. ext_render.compose_frame() 生成 Vec<DrawCmd>
4. host_commit_frame() 写入 FrameSlot::commit() ───► 原子递增 generation 并触发 glib::MainContext 唤醒
   (零等待立即返回，继续处理下一批事件或挂起)          5. DrawingArea draw_func 调用 FrameSlot::latest()
                                                     6. PangoCairo 遍历 DrawCmd 绘制像素到 Wayland/X11 Surface
```

### 7 种平台无关绘制原语（`DrawCmd`） *drawcmd-primitives*

`packages/lava_sys/src/lib.ns` 与 `crates/lava-proto` 共享完全同构的 7 种 2D 场景原语，整个编辑器的分屏边框、行号槽、Git 差异色条、彩虹括号、真彩色色块、波浪下划线、悬浮弹窗、状态栏与光标均由此组合而成：

| `DrawCmd` 变体 | 字段签名 | 渲染职责 |
| --- | --- | --- |
| `ClipPush` | `(x: f64, y: f64, w: f64, h: f64)` | 压入矩形裁剪区域（隔离各分屏窗格、侧边栏与浮窗边界） |
| `ClipPop` | `()` | 弹出最近一层裁剪区域 |
| `FillRect` | `(x, y, w, h, color_rgba: i64, radius: f64)` | 绘制直角或圆角填充矩形（背景、光标行、选区、Toast 卡片、色块） |
| `StrokeRect` | `(x, y, w, h, color_rgba, line_width, radius)` | 绘制描边矩形边框（Telescope / ToggleTerm 浮窗与补全菜单外框） |
| `Line` | `(x1, y1, x2, y2, color_rgba, line_width)` | 绘制分割线、缩进参考线（Indent Guides）与诊断下划线 |
| `TextRun` | `(x, y, text, font_family, font_size_pt, color_rgba, bold, italic)` | 使用 Pango 渲染单段同样式 UTF-8 文本（自动处理 CJK 宽字符与连字） |
| `CursorCaret` | `(x, y, w, h, color_rgba, shape: CaretShape)` | 绘制光标（Normal `Block`、Insert `Beam`、Replace `Underline`、失焦 `HollowBlock`） |

### 为什么 `FrameSlot` 不会积压掉帧？ *frameslot-design*

传统队列式 UI 通道在高频滚屏或后台大量 PTY 输出时容易堆积数十帧过时画面导致输入延迟。`FrameSlot` 只保留**最新提交的一帧 `Arc<RenderFrame>`** 以及单调递增的 64 位 `generation` 计数器：如果 VM 在两次显示器刷新（16.6ms @ 60Hz 或 8.3ms @ 120Hz）之间连续提交了 5 帧，GTK4 主线程在 VSync 到来时仅拉取第 5 帧进行一次 Cairo 光栅化，中间 4 帧零成本自动跳过，保证极致的跟手响应度。

---

## 宿主调用协议全表（`LavaHostHandler`） *host-protocol*

Nessa 代码仅通过 `std.runtime.host_call(op_name, args)` 与 Rust 宿主交互（并由 `@lava_sys`、`@lava_accel`、`@lava_term` 封装为类型安全的 Nessa 函数）。当 VM 调用 `ui_wait_events` 且当前事件队列为空时，`LavaHostHandler` 返回 `HostCallOutcome::SuspendRetry`，将 VM 调度器置于零 CPU 占用的条件变量挂起态，直到键盘鼠标输入、定时器到期、后台子进程完成或 PTY 输出到达时立即唤醒！

| 类别 | 宿主操作名 (`op`) | 功能说明 |
| --- | --- | --- |
| **引导与窗口事件** | `sys_boot_config` | 读取当前会话的命令行 `startup_files`、`headless_keys` 与 `headless` 标志 |
| | `ui_poll_events` | 非阻塞拉取当前待处理的全部 `RawUiEvent` 列表 |
| | `ui_wait_events` | 若队列为空则通过 `SuspendRetry` 零开销挂起至事件到达或超时 |
| | `ui_commit_frame` | 将 `WindowSpec` 与 `DrawCmd` 列表构建为 `RenderFrame` 写入 `FrameSlot` |
| | `ui_measure_font` | 调用 Pango 度量给定字体族与字号的精确等宽单元格宽高与基线（`FontMetrics`） |
| | `ui_clipboard_get` / `ui_clipboard_set` | 读写系统剪贴板（Wayland / X11 原生剪贴板或无头内存缓冲） |
| | `ui_request_quit` | 通知宿主退出主窗口事件循环并记录进程退出码 |
| **文件与异步任务** | `fs_read_lines` / `fs_write_lines` | 高效按行读取与原子落盘保存 UTF-8 文本文件 |
| | `sys_spawn_job` / `sys_cancel_job` | 在后台 OS 线程派生外部编译/Grep 子进程，完成后推送 `RawUiEvent.AsyncJobDone` |
| | `sys_schedule_timer` | 注册毫秒级延时定时器，到期推送 `RawUiEvent.Tick(timer_id, now_ms)` |
| **多核加速算子 (`@lava_accel`)** | `accel_fuzzy_rank` | 对候选集执行词边界/前缀/连续子串加权模糊打分并返回匹配字符下标 |
| | `accel_workspace_files` | 快速递归枚举工作区文件（自动过滤忽略规则与二进制目录） |
| | `accel_workspace_grep` / `accel_workspace_replace` | 多线程并行全文搜索与跨文件精确行号替换 |
| | `accel_diff_lines` | Myers 最小编辑距离行级 Diff（驱动 `ext_gitsigns` 与 `ext_magit`） |
| | `accel_git_status` / `accel_git_head_lines` / `accel_git_blame_line` | 仓库 Porcelain 状态解析、`HEAD` 内容提取与单行提交元数据查询 |
| | `accel_scan_symbols` | 跨 Nessa/Rust/Python/TS/Go/C/Markdown 提取函数、结构体与标题符号树 |
| | `accel_scan_buffer_todos` / `accel_scan_workspace_todos` | 高速扫描缓冲区或全工作区的 `TODO`/`FIXME`/`NOTE`/`HACK`/`PERF`/`WARN`/`BUG` |
| | `accel_index_docs` / `accel_search_docs` | 解析 Lava 与 Nessa Markdown 手册标题节点、`*help-tag*` 锚点并执行五域相关性检索 |
| | `accel_find_symbol_defs` | 在工作区 `.ns`/`.rs` 等源码中精确定位函数、结构体、枚举、效应与常量的定义行号 |
| **伪终端 (`@lava_term`)** | `pty_available` / `pty_spawn` / `pty_kill` | 查询宿主 PTY 能力、派生带真实 TTY 的子 Shell 进程及终止会话 |
| | `pty_write` / `pty_send_key` / `pty_resize` | 写入原始字节、编码发送终端按键序列（含方向键/功能键/Ctrl/Alt）及动态调整行列数 |
| | `pty_snapshot` | 拉取 `VtScreen` 最新可见行、滚回缓冲、光标位置、窗口标题、CWD 与 TrueColor 属性段 |

---

## 定界延续驱动的交互提示与五级按键路由 *keystroke-routing-pipeline*

### 为什么 `ask_prompt` 不会阻塞主事件循环？ *continuation-prompt-engine*

在许多编辑器中，编写需要连续询问用户多个问题的命令（例如 Emacs 的 `query-replace`：先问查找词，再问替换词，再对每一处匹配逐一按 `y`/`n`/`!`/`q` 确认）往往不得不拆成支离破碎的回调状态机。

Lava 利用 Nessa 的 **多提示符定界延续（Multi-Prompt Delimited Continuation）** 彻底消除了回调地狱（代码位于 `packages/lava_core/src/lib.ns`）：

```nessa
pub effect ask_prompt(req: PromptRequest, catch k) -> ?String
```

1. 当任意命令（如 `find-file`、`evil-ex` 或 `query-replace`）执行 `let ans? = ask_prompt(req)# else { return () }` 时，`SystemKernel.dispatch_command` 外围的消除块捕获从该调用点直到命令边界的剩余计算栈段 `k: Continuation`（零帧拷贝栈段分离），将 `(req, k)` 存入 `kernel.active_prompt`，并立即返回 `PromptTurn.Suspended(req, k)`！
2. 顶层事件循环（`ext_app`）毫无阻塞地继续渲染界面、响应窗口缩放与后台消息，同时 `ext_minibuffer` 接管键盘输入，为用户提供实时 Orderless 过滤与 Consult 预览。
3. 当用户按下 `<Return>`（或 `<C-g>` 取消）时，`ext_minibuffer` 调用 `kernel.submit_prompt_answer(answer)`，内核直接调用 `k(answer)` 恢复刚才暂停的栈段！如果后续逻辑再次调用 `ask_prompt`（例如 `query-replace` 的第二次提问），它会再次平滑挂起为新的 `PromptTurn.Suspended`，直至整个命令流自然走完返回 `PromptTurn.Completed`。

### 全局五级按键路由管线（`ext_app::route_stroke_inner`） *stroke-routing-order*

每一次物理按键事件 `RawUiEvent.Key` 进入 `ext_app` 后，按以下确定性优先级依次派发（先消费先返回）：

1. **第 1 级 — 挂起态交互提示符（Minibuffer Continuation）**：只要 `kernel.active_prompt != null`，所有按键无条件交给 `ext_minibuffer.minibuffer_handle_key` 处理（编辑输入、`<C-n>`/`<C-p>` 选词、`<C-g>` 取消回滚、`<Return>` 恢复延续）。
2. **第 2 级 — 进行中的 Emacs 前缀序列**：若 `EmacsState` 正处于未完成的 `C-x ...` / `C-h ...` 前缀等待或 `C-u` 通用数字参数累积状态，直接交由 `ext_emacs.emacs_handle_key` 完成序列。
3. **第 3 级 — 特殊工具模式优先拦截**：当处于无模态或 Evil Normal 空闲态时，优先检查 `ext_tools` 的特殊缓冲区局部交互（如 `*Dired*`、`*Compilation*`、`org` 标题 `<Tab>` 折叠）。
4. **第 4 级 — Evil-Vim 模态引擎与插件拦截器（`ext_vim`）**：
   - 先处理 `<C-z>` 模态/无模态全局切换；
   - 若启用了 `evil_mode`：依次检查活动模态拦截器 `vim.modal_interceptor`（如 Telescope 双浮窗、`TERM-INSERT` 终端直通、Flash 标签跳转、Surround 目标字符读取） -> 多键插件序列 -> Insert/Replace 模式（含 `ext_cmp` 与 `ext_autopairs` 插入拦截器） -> `<Space>` Leader 键位树（自动生成实时 Which-Key 提示） -> `mode:<major_mode>` 缓冲区局部键位（如 `mode:dirvish`、`mode:info`、`mode:magit`、`mode:lazy`） -> Evil 原生计数、操作符、动作与文本对象；
   - 对于未在当前 Vim 模式显式覆盖的 `<M-x>`、`C-x`、`C-h`、`C-s`、`C-g` 等经典 Emacs 组合键，`is_emacs_passthrough_chord` 自动放行至下一级！
5. **第 5 级 — Emacs 全局键位与无模态自插入兜底（`ext_emacs`）**：匹配 `emacs-global` 键位表（`M-x`、`C-x *`、`C-h *`、`C-s`、`M-%` 等）；若当前处于纯 Emacs 无模态（`evil_mode == false`），则进一步执行 `C-f`/`C-b`/`C-n`/`C-p`/`C-a`/`C-e`/`C-k`/`C-y`/`M-y` 及可打印字符直接插入。

想了解如何在这一架构之上编写自己的扩展插件，请继续阅读 `[extension-development#top]`。
