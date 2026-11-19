# Lava — 基于单 Nessa 虚拟机的可编程现代编程环境 *lava-readme* *lava-overview*

**Lava** 是一个运行在 **单一共享 Nessa 虚拟机（Single Nessa VM）** 之上的现代可编程图形化编程环境与混合模态编辑器。它以极薄的 **Rust + GTK4 + PangoCairo** 扁平宿主底座提供原生 Linux 窗口渲染、无锁双缓冲帧交换（`FrameSlot`）、POSIX 伪终端（PTY）与多核并行加速算子，而将整个应用层——包括文档缓冲区、分叉撤销树、分屏与标签页窗口管理器、2D 场景合成器、Evil-Vim 模态状态机、Emacs 无模态/前缀键体系、Vertico/Orderless/Marginalia/Consult 交互式 Minibuffer、Dirvish/Ranger 三栏文件管理器、VT100/TrueColor 终端管理器以及全局自省文档系统——**100% 交由一等公民 Nessa Extension 构建与编排**。

在 Lava 中，没有割裂的“硬编码编辑器内核 vs 受限外部脚本插件”二元边界：所有内置子系统与用户的 `~/.config/lava/init.ns` 脚本运行在同一个进程内 Nessa VM 中，共享同一套 `TypePool`、垃圾回收堆、`SystemKernel` 服务注册表与多提示符定界延续（Multi-Prompt Delimited Continuation）调度器。

快速查阅完整在线手册目录请参阅 `[index#top]`（在编辑器内按 `C-h i` 或执行 `:Info` 即可进入交互式 `*Info*` 超文本浏览器）。

---

## 核心设计原则（8 大架构基石） *lava-eight-principles*

Lava 的每一层代码均严格围绕以下八条架构契约设计（详见深度架构手册 `[architecture#top]`）：

1. **单一共享 Nessa 虚拟机（Single Nessa VM）**：整个应用会话仅启动一个进程内 Nessa VM 实例，所有核心包、40 个内置 Extension 与用户 `init.ns` 统一编译链接至同一张包依赖图并共享内存对象，彻底消除跨进程 IPC 序列化税。
2. **扁平化 Rust/GTK4 底层核心（Flat Rust/GTK4 Core）**：Rust 宿主（`lava-proto`、`lava-gtk`、`lava-vm`、`lava-accel`、`lava-term`、`lava-cli`）仅提供窗口、PangoCairo 2D 光栅化、输入采集、剪贴板、文件系统、POSIX PTY 与 CPU 密集型只读索引加速，绝不内置任何 Vim/Emacs 模式概念或业务布局状态。
3. **Nessa 组织应用层（Nessa-Organized Application Layer）**：缓冲区与分叉 `UndoTree`（`ext_document`）、二叉树分屏与标签页（`ext_window`）、8 套主题调色板与词法高亮合成器（`ext_render`）等应用骨架完全使用 Nessa 编写。
4. **Extension 作为一等架构单元（First-Class Extensions）**：核心编辑体验与扩展插件共享完全对等的 `ExtensionDescriptor` 生命周期契约（拓扑依赖校验、激活钩子、服务发布、按所有者精确回收命令/快捷键/UI 装饰）。
5. **用户脚本与扩展世界完全统一（Unified User Script & Extension World）**：用户的 `~/.config/lava/init.ns` 在启动时动态合成为 `user.local/user_init` Nessa 包，可直接导入 `@lava_core`、`@ext_document`、`@ext_vim` 等任意包，定义结构体、发布服务并注册命令。
6. **基于代数效应与处理器的服务注入（Algebraic Effect & Handler Services）**：日志（`log_event`）、状态栏消息（`status_emit`）、系统剪贴板（`clipboard_get` / `clipboard_set`）、交互式提示输入（`ask_prompt`）与异步后台作业（`spawn_bg_job`）全部建模为静态类型检查的 Nessa `effect` 与 `handles(...)` 能力处理器（详见 `[nessa-for-lava#top]`）。
7. **交互范式完全由扩展定义（Extension-Defined Interaction）**：Evil-Vim 模态编辑（`ext_vim`）与 Emacs 键位体系（`ext_emacs`）均作为普通 Extension 挂载，按 `<C-z>` 可在运行期毫秒级无损切换模态与无模态工作流（详见 `[vim-and-emacs-keybindings#top]`）。
8. **UI 渲染线程与 VM 计算线程彻底解耦（Decoupled UI & VM Threads）**：GTK4 主线程与 Nessa VM 专属工作线程通过原子世代号 `FrameSlot`（单槽最新帧覆盖）与有界事件通道通信；VM 提交 `RenderFrame` 绝不阻塞等待垂直同步，GTK4 重绘绝不阻塞等待 GC 或脚本计算。

---

## 构建、运行与 `.nsbc` 字节码瞬时启动 *build-and-run*

### 编译发布版二进制 *build-release*

在 `/home/fin/apps/lava` 工作区根目录下使用 Cargo 编译高性能发布版二进制：

```bash
cargo build --release
```

编译产物位于 `./target/release/lava`。首次启动或执行 `lava check` 时，`lava-vm` 会将所有核心包、40 个内置 Extension 以及用户 `init.ns` 编译为单一归档字节码，并基于 **128-bit 内容哈希 + 工具链版本指纹**原子写入 `.nsbc` 字节码缓存目录（默认 `target/lava-nsbc-cache/<hash>.nsbc`，可通过环境变量 `LAVA_NSBC_CACHE_DIR` 自定义或设为 `0`/`off` 禁用）。此后再次启动任意文件或注入按键序列时，Lava 直接从 `.nsbc` 缓存零重编译瞬时冷启动，并通过 `sys_boot_config` 宿主调用动态注入命令行参数！

### 常用命令行启动方式 *cli-usage-overview*

```bash
# 启动 GTK4 图形界面并打开文件
./target/release/lava src/main.ns README.md

# 指定工作区根目录与自定义用户配置脚本
./target/release/lava --workspace /path/to/project -u ~/.config/lava/init.ns

# 跳过默认 init.ns 纯净启动（等同于 Emacs -q / Vim -u NONE）
./target/release/lava -q

# 静态校验全部内置与用户 Extension 的单 VM 包图并预热 .nsbc 缓存
./target/release/lava check

# 列出工作区内所有已发现的核心包、内置插件与用户插件
./target/release/lava list-extensions

# 无头模式（Headless）自动回放按键流并将最终 2D 渲染帧导出为 PNG
./target/release/lava --headless \
  --keys "iHello from Lava!<Escape>:w /tmp/demo.txt<Return>:q<Return>" \
  --dump-frame-png /tmp/lava-shot.png
```

完整 CLI 参数说明请参阅用户指南 `[user-guide#top]`。

---

## 核心特性矩阵 *feature-matrix*

| 领域 | 内置能力概览 | 相关扩展 / 模块 | 深入阅读 |
| --- | --- | --- | --- |
| **混合模态编辑** | Normal / Insert / Visual (`v`/`V`/`<C-v>` 矩形块多行同步插入) / Replace / Operator-Pending、嵌套文本对象、`qa`..`@a` 宏、`.` 重复、`M-x`、`C-u` 通用前缀参数、`C-x` 窗口/窄化/矩形块命令、`<C-z>` 一键切换 Evil 与纯 Emacs 模式 | `ext_vim`, `ext_emacs`, `ext_surround`, `ext_comment`, `ext_multicursor`, `ext_flash` | `[vim-and-emacs-keybindings#top]` |
| **分叉历史与寄存器** | 完整 `UndoTree` 分叉撤销树（`u` / `<C-r>` / `g-` / `g+` / `<Space>u` 可视化）、60 槽位 Emacs `KillRing`（`C-y` / `M-y` / `[y` / `]y`）、双模同步 `RegisterBank`（`"` / `+` / `_` / `0` / `1-9` / `a-zA-Z`） | `ext_document`, `ext_yanky` | `[user-guide#top]` |
| **富交互 Minibuffer** | Vertico 纵向候选列表、Orderless 空格多词乱序模糊与 `!排除`/`^前缀`/`后缀$`/`=精确` 语法、Marginalia 右对齐类型徽章与键位反查、Consult 实时缓冲区/主题/搜索预览（`<Escape>`/`<C-g>` 无损回滚）、两阶段 `:Ex` 补全 | `ext_minibuffer` | `[minibuffer-and-completion#top]` |
| **文件浏览与模糊检索** | Telescope 双浮窗检索（文件、实时 Grep、符号、Buffer、键位、配色与文档）、Dirvish + Ranger 三栏 Miller Columns 实时语法预览文件管理器、Oil 可编辑目录缓冲区、Neo-tree 停靠文件树、Harpoon 极速书签 | `ext_telescope`, `ext_dirvish`, `ext_oil`, `ext_neotree`, `ext_harpoon` | `[dirvish-and-ranger#top]` |
| **原生 PTY 与 VT100 终端** | Linux POSIX 主从伪终端（`openpty` + `fork` + `TIOCSWINSZ`）、VT100/ANSI 16/256/TrueColor 状态机与备用屏幕、`1..9` 多会话、`float`/`split`/`vsplit`/`buffer` 四种布局、`TERM-INSERT` 与 `<C-\><C-n>` `TERM-NORMAL` 滚回复制模式 | `lava-term`, `ext_toggleterm` | `[terminal-and-pty#top]` |
| **Git 全工作流套件** | 实时行级 Myers Diff 侧边栏标记与内联 Blame（`ext_gitsigns`）、交互式 `*magit*` 暂存/提交/分支面板（`ext_magit`）、一键合并冲突解决（`ext_git_conflict`） | `ext_gitsigns`, `ext_magit`, `ext_git_conflict` | `[plugins-catalog#top]` |
| **代码智能与工作流** | 多源补全弹窗（`ext_cmp`）+ Tabstop 代码片段（`ext_snippets`）、诊断面板（`ext_trouble`）、符号大纲面包屑（`ext_aerial`）、确定性格式化（`ext_conform`）、全工作区选择性替换（`ext_spectre`）、任务运行器（`ext_overseer`） | `ext_cmp`, `ext_snippets`, `ext_trouble`, `ext_aerial`, `ext_conform`, `ext_spectre`, `ext_overseer` | `[plugins-catalog#top]` |
| **Emacs 自省与全局帮助** | `*Info*` 超文本手册浏览器（`C-h i` / `:Info`）、五域全局全文检索 `apropos-documentation`（`C-h d` / `:AproposDocs`）、命令/选项/键位源码级自省（`C-h f`/`v`/`k`/`m`/`w`/`a`/`o`、`K` 悬停帮助，按 `s` 直跳 `.ns` 源码定义） | `ext_help`, `ext_emacs`, `ext_lazy`, `lava-accel` | `[emacs-help-and-info#top]` |

---

## 高频快捷键速查表 *quick-keybindings*

| 按键 / 命令 | 所属范式 | 功能说明 |
| --- | --- | --- |
| `<Space> f f` / `<Space> s f` / `C-x C-f` | Leader / Emacs | 查找并打开工作区文件（Minibuffer Orderless / Telescope 双浮窗预览） |
| `<Space> s g` / `:Telescope live_grep` | Leader / Ex | 全工作区实时多核并行文本检索（支持 `<C-q>` 导出至 `*Quickfix*`） |
| `<Space> b b` / `<Space> s b` / `C-x b` | Leader / Emacs | 切换已打开缓冲区（带 Consult 实时缓冲区内容预览） |
| `-` / `<Space> f d` / `:Dirvish` / `C-x d` | Normal / Ex / Emacs | 在当前文件所在目录打开 Dirvish 三栏 Miller Columns（Ranger 键位） |
| `<C-\>` / `<Space> t f` / `<Space> t t` | Global / Leader | 一键呼出/隐藏悬浮 POSIX PTY 终端，或打开全窗口编号终端 `*terminal-1*` |
| `<C-\><C-n>`（终端内） | Terminal | 从 `TERM-INSERT` 切换至 `TERM-NORMAL`，使用原生 Vim 移动、搜索与复制输出 |
| `<M-x>` / `:` | Emacs / Evil | 呼出 Emacs 扩展命令面板或两阶段上下文感知 `:Ex` 命令行 |
| `<C-z>` | Global | 在 Evil-Vim 模态编辑与纯 Emacs 无模态编辑之间即时切换 |
| `C-h i` / `:Info` | Emacs Help | 打开 `*Info*` 超文本手册系统，浏览全部 Lava 与 Nessa 官方手册 |
| `C-h d` / `:AproposDocs` / `<Space> s H` | Emacs Help / Telescope | 跨手册、命令、选项、服务与 `.ns` 符号进行五域全局全文搜索 |
| `C-h f` / `C-h v` / `C-h k` / `K` | Emacs Help / Normal | 查看命令文档（可按 `s` 跳转源码）、查看/在线修改选项（按 `e`）、按键反查、光标处自省 |
| `<Space> g g` / `C-x g` | Leader / Emacs | 打开 `*magit*` 交互式 Git 状态面板（`s` 暂存、`c` 提交、`b` 分支） |
| `<Space> l` / `:Lazy` / `:CheckHealth` | Leader / Ex | 打开 `*Lazy*` 单 VM 扩展管理器（启停插件、查看依赖与运行健康检查） |

---

## 仓库目录结构 *workspace-layout*

```text
/home/fin/apps/lava/
├── Cargo.toml                  # Rust Workspace 根配置
├── crates/                     # 扁平化 Rust/GTK4 宿主底座（共 6 个 crate）
│   ├── lava-proto/             # 平台无关 2D 渲染原语 (DrawCmd/RenderFrame)、输入事件与无锁 FrameSlot
│   ├── lava-gtk/               # GTK4 窗口宿主、PangoCairo 渲染管线、Wayland/X11 剪贴板与输入控制器
│   ├── lava-vm/                # 单 Nessa VM 组合器 (LavaWorkspace)、.nsbc 缓存与 LavaHostHandler 桥接
│   ├── lava-accel/             # Rust 多线程只读加速器（模糊打分、并行 Grep、Myers Diff、Git、文档索引）
│   ├── lava-term/              # 原生 Linux POSIX PTY 进程管理与 VT100/ANSI/TrueColor 屏幕状态机
│   └── lava-cli/               # `lava` 命令行入口（GUI 会话、Headless 测试回放、PNG 帧导出、包检查）
├── packages/                   # 核心基础 Nessa 包（共 4 个核心库）
│   ├── lava_sys/               # 底层宿主协议绑定 (`DrawCmd`、`RawUiEvent`、`host_*` 原语与无头模拟桩)
│   ├── lava_core/              # 微内核 `SystemKernel`、代数效应声明 (`ask_prompt` 等)、键位树与钩子总线
│   ├── lava_accel/             # `lava-accel` 原生加速算子的强类型 Nessa 封装与纯 Nessa 回退实现
│   └── lava_term/              # `lava-term` POSIX PTY 会话与 VT100 快照 API 的 Nessa 绑定
├── extensions/                 # 40 个一等公民内置 Nessa 扩展插件 (`ext_document`、`ext_vim`、`ext_help` 等)
├── examples/                   # 示例用户配置与动态扩展脚本 (`examples/init.ns`)
└── docs/                       # 完整中文手册体系（支持 `C-h i` Info 超文本浏览与 `C-h d` 全文检索）
```

---

## 文档导航总览 *documentation-suite*

Lava 内置完整的 11 册中文权威参考手册，既可直接阅读 Markdown 文件，也可在编辑器内通过 `C-h i`（`:Info`）或 `C-h d`（`:AproposDocs`）实时检索：

- `[index#top]` — **手册总目录与阅读路线图**（`docs/index.md`）
- `[architecture#top]` — **架构设计与单 VM 原理**（`docs/architecture.md`）
- `[user-guide#top]` — **日常使用指南、29 项配置与主题定制**（`docs/user-guide.md`）
- `[vim-and-emacs-keybindings#top]` — **Evil-Vim 与 Emacs 混合键位全表**（`docs/vim-and-emacs-keybindings.md`）
- `[minibuffer-and-completion#top]` — **Minibuffer：Vertico、Orderless、Marginalia 与 Consult**（`docs/minibuffer-and-completion.md`）
- `[dirvish-and-ranger#top]` — **Dirvish 三栏文件管理器与 Ranger 快捷键**（`docs/dirvish-and-ranger.md`）
- `[terminal-and-pty#top]` — **原生 POSIX PTY 与 VT100/TrueColor 终端指南**（`docs/terminal-and-pty.md`）
- `[emacs-help-and-info#top]` — **Emacs 全局帮助、`*Info*` 浏览器与 Apropos 自省系统**（`docs/emacs-help-and-info.md`）
- `[plugins-catalog#top]` — **40 个内置扩展插件与 4 大核心包全景图鉴**（`docs/plugins-catalog.md`）
- `[extension-development#top]` — **Nessa 扩展插件开发实战指南**（`docs/extension-development.md`）
- `[nessa-for-lava#top]` — **面向 Lava 用户的 Nessa 语言与代数效应速成手册**（`docs/nessa-for-lava.md`）

---

## 验证与自动化测试套件 *testing-commands*

Lava 从底层协议、单 VM 组合器、终端解析器到全量 40 个 Nessa 扩展均配备端到端确定性测试：

```bash
# 运行全部 Rust 宿主与单 VM 集成测试（涵盖 lava-proto / lava-vm / lava-accel / lava-term / lava-cli）
cargo test --workspace

# 校验整个单 VM 扩展依赖图并预热 .nsbc 字节码缓存
./target/release/lava check

# 使用自定义示例配置执行无头端到端按键流与渲染测试
./target/release/lava -u examples/init.ns --headless \
  --keys "<Space>nnDemoNote<Return>:q!<Return>"
```

---

## 开源协议

本项目采用 **Mozilla Public License Version 2.0 (MPL-2.0)** 开源协议，完整协议文本见 [LICENSE](LICENSE)。
