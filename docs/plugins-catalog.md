# 核心包、全部 45 个内置扩展与独立服务端集合全景目录 *plugins-catalog*

Lava 构建在 **“单一 Nessa 虚拟机（Single Nessa VM）+ 扁平原生底座 + 一等公民扩展体系 + 独立无头服务端集合”** 之上：除 GTK4/PangoCairo 渲染壳、POSIX PTY、非阻塞 JSON-RPC LSP/SSH 守护池与多核计算加速器由 Rust 提供外，编辑器的缓冲世界、分屏窗口、渲染合成、Evil-Vim 与 Emacs 双模态键位、Minibuffer、LSP 语言智能与 18 语言预设、SSH Remote 全透明远程工程与服务端 LSP、基于 stdio `Mapper` / `Filter`（`sed`/`awk`/`rg`）的高级 Diff 引擎、Git 工作流、文件管理器、终端多路复用乃至全局帮助系统，**全部由运行在同一个 Nessa VM 内的 6 个核心包与 45 个一等内置扩展实现**；同时，远端主机侧配套提供一套**独立的无头 Nessa 服务端集合 `lava.server/*`（独立二进制 `lava-server`，含 2 个服务端核心包与 6 个服务端扩展）**。

所有客户端扩展共享同一个 `SystemKernel` 服务注册表、代数效应处理器与事件钩子总线，彼此之间是零序列化、零进程间通信（Zero-IPC）的直接类型化调用；同时，用户的 `~/.config/lava/init.ns` 与内置扩展处于完全对等的语言世界中。

相关阅读：[architecture#top]、[extension-development#top]、[emacs-help-and-info#top]、[vim-and-emacs-keybindings#top]、[nessa-for-lava#top]。

---

## 扩展运行时查看与生命周期管理 *managing-extensions*

你可以通过命令行或编辑器内自省工具随时查看和管理全部 45 个客户端内置扩展与 6 个独立服务端扩展：

- **命令行静态拓扑列表**：运行 `lava list-extensions` 打印按依赖拓扑排序的全部客户端核心包与扩展 ID；运行 `lava check` 验证客户端全部 45 个扩展与 `init.ns` 的单 VM 编译健康度；运行 `lava-server list-extensions` 与 `lava-server check` 验证独立服务端集合 `server/`。
- **`*Lazy*` 交互式扩展管理器（`<Space> l` / `:Lazy`）**：由 `lava.org/ext_lazy` 提供，支持按 `<Return>` / `K` 查看扩展详情、按 `x` / `e` 动态启停扩展、按 `R` 激活全部扩展，以及按 `c`（或执行 `:CheckHealth`）运行全量内核与运行时健康诊断。
- **Emacs 自省与全局搜索（`C-h p` / `C-h d`）**：按 `C-h p`（`describe-extensions`）查看实时内核服务表与扩展图；按 `C-h d`（`apropos-documentation`）跨扩展、服务、Hook 与命令执行全文检索（详见 [emacs-help-and-info#top]）。

---

## 六大客户端核心基础包（`packages/*`） *core-packages*

在加载任何 `ext_*` 扩展之前，组合编译器（`crates/lava-vm`）首先链接位于 `packages/` 目录下的 6 个基础 Nessa 包，它们定义了宿主协议、微内核、原生加速以及远程/LSP 传输绑定：

| 包标识（Package ID） | 源码路径 | 核心职责与关键导出 API |
| :--- | :--- | :--- |
| `lava.org/lava_sys` | `packages/lava_sys/src/lib.ns` | **宿主 ABI 与无头模拟协议**：定义 `DrawCmd` 2D 渲染指令、`RawUiEvent` 键鼠/尺寸/PTY 事件、`FontMetrics`、`WindowSpec` 以及 `host_*` 原生桥接原语与 `--headless` 纯内存桩。 |
| `lava.org/lava_core` | `packages/lava_core/src/lib.ns` | **微内核与代数效应底座**：提供 `SystemKernel`、`ExtId`、`ExtensionDescriptor`、`Keymap`、`CommandSpec`、`MacroRecorder`，以及基于多提示符定界延续（`catch k`）的 `ask_prompt`、`spawn_bg_job`、`sleep_ticks`、`status_emit`、`clipboard_get`/`set` 效应接口。 |
| `lava.org/lava_accel` | `packages/lava_accel/src/lib.ns` | **多核 Rust 原生加速绑定**：封装 `accel_fuzzy_rank`、`accel_workspace_files`、`accel_workspace_grep`、`accel_workspace_replace`、`accel_diff_lines`、`accel_git_*`、`accel_scan_symbols`、`accel_scan_*_todos`、`accel_index_docs`、`accel_search_docs`、`accel_find_symbol_defs`，并内置纯 Nessa 回退实现。 |
| `lava.org/lava_term` | `packages/lava_term/src/lib.ns` | **Linux POSIX PTY 与 VT100 屏幕快照绑定**：导出 `VtScreen`、`VtRowSpan` 与 `pty_available`、`pty_spawn`、`pty_write`、`pty_send_key`、`pty_resize`、`pty_snapshot`、`pty_kill`，完整响应 DA1/DA2/DSR/CPR/Kitty/OSC/DCS 终端能力查询，驱动零轮询真终端仿真。 |
| `lava.org/lava_lsp` | `packages/lava_lsp/src/lib.ns` | **非阻塞 JSON-RPC 2.0 LSP 与远端 URI 双向映射绑定**：导出 `LspDiagnostic`、`LspLocation`、`LspCompletion`、`LspSymbol`、`LspTextEdit` 与 `lsp_start`、`lsp_spawn_remote`、`lsp_stop`、`lsp_did_open/change/save/close`、`lsp_hover`、`lsp_definition`、`lsp_references`、`lsp_rename`、`lsp_code_action`、`lsp_format`、`lsp_poll_diagnostics`、`lsp_find_binary`，支持服务端 LSP 进程（如远端 `clangd`）与 `ssh://host/path` <-> `file:///path` 双向无损 URI 转换。 |
| `lava.org/lava_remote` | `packages/lava_remote/src/lib.ns` | **SSH 远程传输与 `lava-server` 多路复用客户端绑定**：支持 `ssh://[user@]host[:port]/path` 与 Emacs TRAMP `/ssh:[user@]host[#port]:/path` URI 解析、`~/.ssh/config` 主机自动发现、持久 `lava-server --stdio` JSON-RPC 2.0 传输与免安装 POSIX SSH ControlMaster 自动回退、远端文件读写/目录树/全文检索/命令执行、SSH `-L`/`-R` 端口转发隧道与远端 LSP 启动编排。 |

---

## 全部 45 个客户端内置扩展速查矩阵 *builtin-plugins*

下表按 11 大功能领域汇总了 `extensions/` 目录下的全部 **45 个内置扩展** 及其发布的核心内核服务键（`SERVICE_*`）：

| 领域分类 | 数量 | 包含扩展（`lava.org/ext_*`） |
| :--- | :---: | :--- |
| **1. 核心底座与双模态引擎** | 7 | `ext_app`、`ext_document`、`ext_window`、`ext_render`、`ext_minibuffer`、`ext_vim`、`ext_emacs` |
| **2. 结构化编辑与高速跳转** | 7 | `ext_autopairs`、`ext_comment`、`ext_surround`、`ext_treesj`、`ext_multicursor`、`ext_flash`、`ext_yanky` |
| **3. 模糊检索、书签与文件浏览器** | 5 | `ext_telescope`、`ext_harpoon`、`ext_dirvish`、`ext_oil`、`ext_neotree` |
| **4. Git 版本控制与管道式 Diff 套件** | 4 | `ext_gitsigns`、`ext_magit`、`ext_git_conflict`、`ext_diff` |
| **5. LSP 语言智能、预设、诊断与格式化** | 5 | `ext_lsp`、`ext_lspconfig`、`ext_trouble`、`ext_aerial`、`ext_conform` |
| **6. 自动补全、代码片段与全局替换** | 3 | `ext_cmp`、`ext_snippets`、`ext_spectre` |
| **7. 状态栏、标签侧栏、通知中心与禅模式** | 4 | `ext_lualine`、`ext_tablist`、`ext_noice`、`ext_zen` |
| **8. 语义注释徽章与高亮增强** | 3 | `ext_todo_comments`、`ext_colorizer`、`ext_illuminate` |
| **9. 终端、任务编排、Emacs 工具集与会话** | 4 | `ext_toggleterm`、`ext_overseer`、`ext_tools`、`ext_persistence` |
| **10. 全局文档自省与扩展管理** | 2 | `ext_help`、`ext_lazy` |
| **11. SSH 远程开发与服务端 LSP 协同** | 1 | `ext_remote` |

---

## 领域一：核心底座与双模态引擎（7 个扩展） *core-extensions*

这 7 个核心扩展按严格拓扑顺序最先初始化，构成编辑器的文档状态、窗口分割树、主题渲染管线、Minibuffer 提示引擎以及 Evil-Vim / Emacs 混合交互模型。

### 1. `lava.org/ext_app` — 顶层会话编排器与帧提交主循环 *ext-app*

- **注册入口与服务**：`register_app_extension` / `register_default_extensions`（编排全部 40 个默认扩展的激活顺序，不额外暴露可变状态服务）。
- **核心命令与键位**：`redraw-display`（在 Emacs 中绑定 `<C-l>` 居中并强制重绘）。
- **核心职责**：接管宿主传入的 `RawUiEvent` 流（键盘、鼠标滚轮/点击、窗口缩放、PTY 异步可读唤醒与后台 tick），驱动按键路由流水线并在每次事件处理后向 `FrameSlot` 提交最新 2D `DrawCmd` 场景帧。

### 2. `lava.org/ext_document` — 文档缓冲世界、分支撤销树与寄存器库 *ext-document*

- **注册入口与服务**：`register_document_extension` | 服务键：`SERVICE_EDITOR_WORLD = "lava.document.world"`（`EditorWorld`）。
- **核心命令与键位**：`save-buffer`、`undo`（`u` / `<C-_>`）、`redo`（`<C-r>`）、`undo-tree-visualize`（`<Space> u`）、`narrow-to-region`（`C-x n n`）、`widen`（`C-x n w`）、`next-buffer`（`:bn`）、`previous-buffer`（`:bp`）。
- **核心职责**：管理所有打开的 `Buffer`、全部 **29 项 `EditorOptions`**（详见 [user-guide#top]）、不丢失任何历史分支的 `UndoTree`、Emacs `KillRing`、Vim/Emacs 共享 `RegisterBank`、Narrowing 视图收窄、跳转历史栈（Jump List）与全局 Quickfix 队列。

### 3. `lava.org/ext_window` — 二叉分屏树、多标签工作区与几何焦点导航 *ext-window*

- **注册入口与服务**：`register_window_extension` | 服务键：`SERVICE_WINDOW_MANAGER = "lava.window.manager"`（`WindowManager`）。
- **核心命令与键位**：`split-window-below`（`<C-w> s` / `<Space> w s` / `C-x 2`）、`split-window-right`（`<C-w> v` / `<Space> w v` / `C-x 3`）、`delete-window`（`<C-w> q` / `<Space> w d` / `C-x 0`）、`delete-other-windows`（`<C-w> o` / `C-x 1`）、`other-window`（`<C-w> w` / `C-x o`）、`window-focus-left/down/up/right`（`<C-w> h/j/k/l`）、`balance-windows`（`<C-w> =`）、`enlarge-window` / `shrink-window`、`scroll-center/top/bottom`（`zz` / `zt` / `zb`）、`tab-new` / `tab-next` / `tab-previous` / `tab-close`（`gt` / `gT` / `:tabnew` / `:tabclose`）。
- **核心职责**：维护递归二叉分割窗格树（Binary Split-Tree）、多 Tab 工作区、浮动窗口层叠坐标以及精确到字符网格的鼠标命中测试（Hit-Testing）。

### 4. `lava.org/ext_render` — 8 套色彩主题、多语言词法高亮与 2D 合成器 *ext-render*

- **注册入口与服务**：`register_render_extension` | 服务键：`SERVICE_RENDERER = "lava.render.compositor"`（`RenderCompositor`）。
- **核心命令与键位**：`set-theme`（`:colorscheme <name>`）、`cycle-theme`。
- **核心职责**：内置 8 套精选调色板（`catppuccin-mocha`、`tokyo-night`、`rose-pine`、`kanagawa`、`nord`、`gruvbox-dark`、`one-dark`、`solarized-light`），提供 Nessa、Rust、Python、C/C++、Go、TypeScript、JSON、TOML、Markdown、Shell 等多语言词法高亮器，并将侧边栏、分屏窗格、行号槽、Git/诊断标记、虚拟文本、浮动面板与状态栏合成为紧凑的 `DrawCmd` 批次。

### 5. `lava.org/ext_minibuffer` — Vertico + Orderless + Marginalia + Consult 提示引擎 *ext-minibuffer*

- **注册入口与服务**：`register_minibuffer_extension` | 服务键：`SERVICE_MINIBUFFER = "lava.minibuffer.controller"`（`MinibufferController`）。
- **核心交互键位**：Minibuffer 激活时支持 `<Up>` / `<Down>` / `<C-n>` / `<C-p>` 候选导航、`<Tab>` 阶段推进补全、`<M-p>` / `<M-n>` 按类别历史回溯、`<C-w>` 删词、`<Escape>` / `<C-g>` 取消并自动回滚预览（详见 [minibuffer-and-completion#top]）。
- **核心职责**：接管 `ask_prompt` 定界延续挂起请求与 `:Ex` 命令行输入，提供 Vertico 纵向候选窗、Orderless 多词与 `!排除` 匹配、Marginalia 右对齐元数据注解、Consult 文件/缓冲区/主题实时预览，以及 `:e`、`:b`、`:set`、`:colorscheme`、`:Telescope`、`:h`、`:Info`、`:Term` 的双阶段上下文智能补全。

### 6. `lava.org/ext_vim` — 完整 Evil-Vim 模态编辑、文本对象与 Ex 命令引擎 *ext-vim*

- **注册入口与服务**：`register_vim_extension` | 服务键：`SERVICE_VIM = "lava.vim.state"`（`VimState`）。
- **核心命令与键位**：`evil-ex`（`:`）、`evil-search-forward`（`/`）、`evil-search-backward`（`?`）、`evil-toggle-mode`（`<C-z>`），以及全套 Normal / Insert / Visual（Char `v`、Line `V`、Block `<C-v>`）/ Replace（`R`）/ Operator-Pending 模式动作、嵌套文本对象（`iw`/`aw`/`i"`/`i(`/等）、`.` 重复、宏录制（`qa`..`q` / `@a` / `@@`）、局部/全局标记（`ma` / `'a` / `` `a ``）与 `<Space>` Leader 路由（详见 [vim-and-emacs-keybindings#top]）。
- **核心职责**：驱动完整的 Vim 模态状态机、模态拦截器栈（Modal Interceptor）与可扩展 `:Ex` 命令解析分发器。

### 7. `lava.org/ext_emacs` — Emacs 经典命令集、增量搜索与无模态混合层 *ext-emacs*

- **注册入口与服务**：`register_emacs_extension` | 服务键：`SERVICE_EMACS = "lava.emacs.state"`（`EmacsState`）。
- **核心命令与键位**：共注册 48 个正统 Emacs 命令，涵盖 `<M-x>`（`execute-extended-command`）、`<C-u>`（`universal-argument` 前缀计数）、`<C-g>`（`keyboard-quit`）、`<C-z>`（`toggle-evil-mode` 切换纯 Emacs 无模态编辑）、`<C-s>` / `<C-r>`（`isearch-forward/backward`）、`<M-%>`（基于延续的交互式 `query-replace`）、`C-x C-f` / `C-x C-s` / `C-x b` / `C-x k` / `C-x C-b`（`ibuffer`）、`C-x r` 矩形与寄存器族、`C-x (` / `C-x )` / `C-x e` 键盘宏族以及 `<C-y>` / `<M-y>` Kill-Ring 轮转粘贴。
- **核心职责**：让 Emacs 核心工作流与 Evil-Vim 在同一套缓冲区、撤销树与剪贴板环上无缝共存。

---

## 领域二：结构化编辑与高速跳转（7 个扩展） *editing-and-navigation-extensions*

### 8. `lava.org/ext_autopairs` — 上下文感知括号/引号自动配对 *ext-autopairs*

- **注册入口与服务**：`register_autopairs_extension`（监听插入模式按键事件，无独立外部服务键）。
- **核心命令**：`autopairs-toggle`（`:M-x autopairs-toggle` 动态开关）。
- **核心职责**：在 Insert 模式中提供 `()`、`[]`、`{}`、`""`、`''`、`` `` `` 的上下文感知自动闭合、右闭合符智能越过（Step-over）、花括号内对称空格填充以及配对退格同步删除。

### 9. `lava.org/ext_comment` — 多语言行注释与块注释切换 *ext-comment*

- **注册入口与服务**：`register_comment_extension`。
- **核心命令与键位**：`comment-toggle-line`（Normal `gcc` / `<Space> /`）、`comment-toggle-visual`（Visual `gc`）、`comment-toggle-block`（Normal `gbc`）、`comment-toggle-block-visual`（Visual `gb`）。
- **核心职责**：自动识别当前缓冲区语言注释语法（如 Nessa `--`、Rust/C/TS `//` 与 `/* */`、Python/TOML/Shell `#`、HTML/Markdown `<!-- -->`），支持带计数的整行注释切换与跨行块注释包裹，并保持最小公共缩进对齐。

### 10. `lava.org/ext_surround` — 定界符与 HTML/XML 标签环绕编辑 *ext-surround*

- **注册入口与服务**：`register_surround_extension`。
- **核心命令与键位**：`surround-add`（`ys<motion><char>`，如 `ysiw"`）、`surround-line`（`yss<char>`）、`surround-delete`（`ds<char>`，如 `ds)`）、`surround-change`（`cs<old><new>`，如 `cs"'`）、`surround-visual`（Visual 模式 `S<char>`）。
- **核心职责**：复刻 `vim-surround` / `nvim-surround` 体验，区分开括号带内边距（`(`）与闭括号紧凑包裹（`)`），并完整支持 HTML/XML 标签（`t`）的包裹、删除与重命名。

### 11. `lava.org/ext_treesj` — 结构化拆合、智能递增递减与行块移动 *ext-treesj*

- **注册入口与服务**：`register_treesj_extension` | 服务键：`SERVICE_TREESJ = "lava.treesj.structural"`。
- **核心命令与键位**：`treesj-toggle`（`<Space> j` / `:TSJToggle`）、`treesj-split`（`:TSJSplit`）、`treesj-join`（`:TSJJoin`）、`dial-increment`（`<C-a>` / `<Space> +` / `<Space> =` / `:DialIncrement`）、`dial-decrement`（`g<C-x>` / `<Space> -` / `:DialDecrement`）、`move-line-down`（`<M-j>` / `:MoveLineDown`）、`move-line-up`（`<M-k>` / `:MoveLineUp`）。
- **核心职责**：三合一结构化编辑利器——一键将单行参数/数组/字典列表展开为多行尾随逗号格式或合并回单行（`treesj`）；支持整数、十六进制、`true`/`false`、`and`/`or`、日期等语义词元循环递增递减（`dial.nvim`）；支持单行或可视选区上下平滑搬移。

### 12. `lava.org/ext_multicursor` — 同步多光标编辑引擎 *ext-multicursor*

- **注册入口与服务**：`register_multicursor_extension` | 服务键：`SERVICE_MULTICURSOR = "lava.multicursor.state"`。
- **核心命令与键位**：`mc-add-next-match`（`<C-n>` / `:MCAddNext`）、`mc-skip-match`（`:MCSkip`）、`mc-add-cursor-down`（`<Space> m j` / `:MCAddDown`）、`mc-add-cursor-up`（`<Space> m k` / `:MCAddUp`）、`mc-add-all-matches`（`<Space> m a` / `:MCSelectAll`）、`mc-clear`（`<Escape>` / `:MCClear`）。
- **核心职责**：支持按当前光标词逐个选中下一个匹配（`<C-n>`）、一次性选中全文所有匹配（`<Space> m a`）或垂直上下派生光标，让多处光标同步执行移动、操作符修改与实时字符插入，并合并为单步原子撤销。

### 13. `lava.org/ext_flash` — 全视口 2D 无冲突标签闪电跳转 *ext-flash*

- **注册入口与服务**：`register_flash_extension` | 服务键：`SERVICE_FLASH = "lava.flash.state"`。
- **核心命令与键位**：`flash-jump` / `flash`（Normal/Visual `gs` / `:Flash`）、`flash-treesitter`（`gS` / `:FlashTreesitter`）。
- **核心职责**：复刻 `flash.nvim`：按下 `gs` 输入任意搜索模式后，自动排除后续可能继续输入的字符，在全屏所有命中位置渲染高对比度单键跳转标签，再按对应字母一键直达目标行列；`gS` 则为当前光标外围的嵌套括号/代码块结构节点分配标签以实现一键结构化选区扩展。

### 14. `lava.org/ext_yanky` — 粘贴后复制环回溯与历史选择器 *ext-yanky*

- **注册入口与服务**：`register_yanky_extension` | 服务键：`SERVICE_YANKY = "lava.yanky.ring"`。
- **核心命令与键位**：`yanky-cycle-prev`（`[y` / `:YankyCyclePrev`）、`yanky-cycle-next`（`]y` / `:YankyCycleNext`）、`yanky-history` / `yanky-ring-history`（`<Space> p` / `:YankyHistory`）、`yanky-clear`（`:YankyClearHistory`）。
- **核心职责**：在每次 `y` 复制时短暂高亮复制区域以提供视觉反馈，允许在 `p`/`P` 粘贴后通过 `[y` / `]y` 原位切换更早或更新的复制环条目，并提供带内容预览的 `<Space> p` 历史粘贴面板。

---

## 领域三：模糊检索、书签与文件浏览器（5 个扩展） *navigation-and-browsers*

### 15. `lava.org/ext_telescope` — 多核模糊查找、全文检索与实时浮动预览面板 *ext-telescope*

- **注册入口与服务**：`register_telescope_extension` | 服务键：`SERVICE_TELESCOPE = "lava.telescope.state"`。
- **核心命令与键位**：`:Telescope <mode>`、`telescope-find-files`（`<Space> s f`）、`telescope-live-grep`（`<Space> s g`）、`telescope-buffers`（`<Space> s b`）、`telescope-symbols`（`<Space> s s`）、`telescope-help-tags`（`<Space> f h`）、`telescope-docs`（`<Space> s H` / `<Space> h s`）、`telescope-commands`（`<Space> f C`）、`telescope-keymaps`（`<Space> s k`）、`telescope-colorscheme`（`<Space> s c`）、`telescope-resume`（`<Space> s r`），面板内按 `<C-q>` 导出至 Quickfix。
- **核心职责**：调用 `lava-accel` 多核并行扫描文件、全文正则/子串、符号与全部手册文档，提供双栏浮动选择器与语法高亮实时上下文预览。

### 16. `lava.org/ext_harpoon` — 高频项目文件与光标槽位瞬时切换器 *ext-harpoon*

- **注册入口与服务**：`register_harpoon_extension` | 服务键：`SERVICE_HARPOON = "lava.harpoon.state"`。
- **核心命令与键位**：`harpoon-add`（`<Space> h a` / `:HarpoonAdd`）、`harpoon-menu`（`<Space> h m` / `:Harpoon`）、`harpoon-nav-1..4`（`<Space> 1`、`<Space> 2`、`<Space> 3`、`<Space> 4`）、`harpoon-next` / `harpoon-prev`（`<Space> h n` / `<Space> h p`）、`harpoon-clear`、`harpoon-save`、`harpoon-load`。
- **核心职责**：将当前开发中反复切换的 1–4 个核心源文件及精确行列位置钉在固定数字槽位上，实现单键零思考瞬移，并按项目工作区持久化为 JSON。

### 17. `lava.org/ext_dirvish` — 三栏 Miller Columns 与全套 Ranger 文件管理器 *ext-dirvish*

- **注册入口与服务**：`register_dirvish_extension` | 服务键：`SERVICE_DIRVISH = "lava.dirvish.state"`。
- **核心命令与键位**：共 48 个命令；全局通过 `-`（`dirvish-jump`）、`<Space> f d`、`<Space> R`、`C-x d` 或 `:Dirvish` / `:Ranger` 打开；内部完整实现 `h`/`j`/`k`/`l` 层级穿梭、`gh`/`gr`/`gt`/`ge` 快速书签、`<Space>` 标记、`v` 反选、`uv` 清空、`yy`/`dd`/`pp`/`po`/`dD` 复制剪切粘贴删除、`cw` 重命名、`a` 新建文件或 `dir/`、`zh` 隐藏文件、`zp` 预览开关、`zf` 过滤、`o` 排序循环与 `S` 在当前目录打开 PTY 终端（详见 [dirvish-and-ranger#top]）。
- **核心职责**：融合 Emacs Dirvish 的极简缓冲区语义与 Ranger 的三栏（父目录 / 当前目录 / 语法高亮文件或子目录实时预览）高效文件操作体验。

### 18. `lava.org/ext_oil` — 可像编辑普通文本一样修改目录的缓冲区文件浏览器 *ext-oil*

- **注册入口与服务**：`register_oil_extension` | 服务键：`SERVICE_OIL = "lava.oil.state"`。
- **核心命令与键位**：`oil` / `oil-open-parent`（`<Space> f o` / `:Oil`；Oil 缓冲区内 `-` 返回上级、`<Return>` 进入、`H` 切换隐藏文件）。
- **核心职责**：复刻 `oil.nvim`：每条目录项带有确定性 `/NNN` 追踪 ID，你可以直接用 Vim 的 `cw`、`dd`、`yy`/`p`、多光标或正则替换批量改名、复制、移动或删除文件，输入 `:w` 时自动生成原子文件系统变更计划并在确认后执行。

### 19. `lava.org/ext_neotree` — 左侧停靠树状项目资源管理器 *ext-neotree*

- **注册入口与服务**：`register_neotree_extension` | 服务键：`SERVICE_NEOTREE = "lava.neotree.state"`。
- **核心命令与键位**：`neotree` / `neotree-toggle`（`<Space> n t`）、`neotree-focus`（`<Space> n f`）、`neotree-reveal`（`<Space> n r` 定位当前文件）、`neotree-add`、`neotree-rename`、`neotree-delete`。
- **核心职责**：提供常驻左侧的树状文件夹折叠/展开侧边栏，内置 Git 工作区变更状态徽章（`M`/`A`/`?`）、鼠标单击展开或打开文件以及目录增删改操作。

---

## 领域四：Git 版本控制套件（3 个扩展） *git-suite-extensions*

### 20. `lava.org/ext_gitsigns` — 实时行级变更槽标记、行内 Blame 与 Hunk 暂存 *ext-gitsigns*

- **注册入口与服务**：`register_gitsigns_extension` | 服务键：`SERVICE_GITSIGNS = "lava.gitsigns.state"`。
- **核心命令与键位**：`gitsigns-next-hunk`（`]c`）、`gitsigns-prev-hunk`（`[c`）、`gitsigns-toggle-blame`（`<Space> g b`）、`gitsigns-stage-hunk`（`<Space> h s`）、`gitsigns-reset-hunk`（`<Space> h r`）、`gitsigns-preview-hunk`（`<Space> h p`）、`gitsigns-refresh`。
- **核心职责**：调用 `accel_git_head_file` 与 Myers 差分引擎 `accel_diff_lines` 在行号槽实时绘制新增/修改/删除色条，支持浮动窗预览单个 Hunk 差异、局部暂存或回滚以及行尾虚拟文本 Git Blame。

### 21. `lava.org/ext_magit` — 交互式 Git 状态面板、分段暂存与提交工作流 *ext-magit*

- **注册入口与服务**：`register_magit_extension` | 服务键：`SERVICE_MAGIT = "lava.magit.state"`。
- **核心命令与键位**：`magit-status` / `magit`（`<Space> g g` / `C-x g`）；在 `*magit*` 缓冲区中：`<Tab>` 折叠/展开 Diff、`s`/`S` 暂存文件或全部、`u`/`U` 取消暂存、`x` 丢弃改动、`c` 编写提交说明并 Commit、`b` 切换或创建分支、`l` 查看提交日志、`<Return>` 访问对应文件行、`g` 刷新、`q` 退出。
- **核心职责**：将 Emacs 王牌插件 Magit 的全键盘分段 Git 操作模型原生带入 Lava。

### 22. `lava.org/ext_git_conflict` — 合并冲突高亮、跳转与单键消解器 *ext-git-conflict*

- **注册入口与服务**：`register_git_conflict_extension` | 服务键：`SERVICE_GIT_CONFLICT = "lava.git_conflict.state"`。
- **核心命令与键位**：`git-conflict-next`（`]x`）、`git-conflict-prev`（`[x`）、`git-conflict-ours`（`co` / `<Space> c o` / `<Space> g o`）、`git-conflict-theirs`（`<Space> c t` / `<Space> g t`）、`git-conflict-both`（`<Space> c b` / `<Space> g B`）、`git-conflict-none`（`<Space> c 0` / `<Space> g 0`）、`git-conflict-qf`（`<Space> c q` / `<Space> g q` 导出全部冲突至 Quickfix）。
- **核心职责**：自动检测 `<<<<<<<` / `=======` / `>>>>>>>` 冲突块并以醒目背景区分 Ours 与 Theirs，支持一键保留任一侧、合并两侧或清空冲突块。

---

## 领域五：诊断、符号大纲与格式化（3 个扩展） *diagnostics-outline-format*

### 23. `lava.org/ext_trouble` — 实时语法/冲突/风格诊断与 `*Trouble*` 面板 *ext-trouble*

- **注册入口与服务**：`register_trouble_extension` | 服务键：`SERVICE_TROUBLE = "lava.trouble.diagnostics"`。
- **核心命令与键位**：`trouble-next`（`]d`）、`trouble-prev`（`[d`）、`trouble-toggle` / `trouble`（`<Space> x x`）；面板内 `<Return>` 跳转、`f` 切换严重度过滤、`r` 刷新、`q` 关闭。
- **核心职责**：实时聚合括号配平检查、合并冲突标记、代码风格与编译器 Quickfix 诊断，在编辑区渲染行号槽图标、波浪下划线与行尾虚拟文本，并提供可过滤的底部 `*Trouble*` 汇总面板。

### 24. `lava.org/ext_aerial` — 右侧符号大纲树与顶部面包屑导航栏 *ext-aerial*

- **注册入口与服务**：`register_aerial_extension` | 服务键：`SERVICE_AERIAL = "lava.aerial.outline"`。
- **核心命令与键位**：`aerial-toggle` / `aerial`（`<Space> c s`）、`aerial-next-symbol`（`g]` / `<Space> s ]`）、`aerial-prev-symbol`（`g[` / `<Space> s [`）、`aerial-jump`（`<Space> c j` / 侧栏 `<Return>`）。
- **核心职责**：调用 `accel_scan_symbols` 提取函数、结构体、枚举、Trait、Effect 与 Markdown 标题，在窗格顶部渲染实时随光标更新的层级面包屑（Winbar Breadcrumbs），并提供右侧停靠大纲侧边栏。

### 25. `lava.org/ext_conform` — 确定性代码格式化与保存时自动格式化 *ext-conform*

- **注册入口与服务**：`register_conform_extension` | 服务键：`SERVICE_CONFORM = "lava.conform.formatter"`（支持 `register_formatter` 与 `register_external_formatter` 扩展注册）。
- **核心命令与键位**：`conform-format` / `format`（`<Space> c f`）、`format-on-save-toggle`（`<Space> u f`）、`conform-info`。
- **核心职责**：内置确定性缩进清理、尾随空白剥离与 JSON 美化器，并可对接 `rustfmt` 等外部格式化工具；支持整文件或 Visual 选区局部格式化，所有改动合并为单步 `UndoTree` 节点且自动保持光标相对位置。

---

## 领域六：自动补全、代码片段与全局替换（3 个扩展） *completion-snippets-replace*

### 26. `lava.org/ext_cmp` — 多源实时模糊自动补全弹窗引擎 *ext-cmp*

- **注册入口与服务**：`register_cmp_extension` | 服务键：`SERVICE_CMP = "lava.cmp.engine"`（开放 `register_source` 供任意扩展注入自定义补全源）。
- **核心命令**：`cmp-complete`、`cmp-confirm`、`cmp-abort`、`cmp-toggle`（Insert 模式下输入字符自动触发浮动候选菜单，支持 `<C-n>` / `<C-p>` / `<Tab>` / `<Return>` 选择确认）。
- **核心职责**：融合 4 大内置补全源——代码片段（Snippets）、当前与已打开缓冲区标识符、语言关键字以及相对文件路径，通过 `accel_fuzzy_rank` 实时排序渲染跟随光标的紧凑补全弹窗。

### 27. `lava.org/ext_snippets` — 多跳点（Tabstop）模板展开引擎 *ext-snippets*

- **注册入口与服务**：`register_snippets_extension` | 服务键：`SERVICE_SNIPPETS = "lava.snippets.registry"`（开放 `register_snippet` 供 `init.ns` 自定义片段）。
- **核心命令与键位**：`snippet-expand-or-jump`（Insert/Normal `<C-k>`）、`snippet-jump-next`、`snippet-jump-prev`（`<C-j>`）、`snippets-list` / `snippets`。
- **核心职责**：内置 Nessa、Rust、Python、TypeScript、Go、Markdown 等常用代码结构片段库，支持 `$1`、`${2:placeholder}`、`$0` 多跳点会话展开与前后跳点穿梭。

### 28. `lava.org/ext_spectre` — 交互式工作区搜索与选择性多文件替换面板 *ext-spectre*

- **注册入口与服务**：`register_spectre_extension` | 服务键：`SERVICE_SPECTRE = "lava.spectre.panel"`。
- **核心命令与键位**：`spectre-open` / `spectre`（`<Space> s R`）；在 `*Spectre*` 面板内：`s` 修改搜索词、`r` 修改替换词、`f` 设置路径过滤、`I` 切换大小写敏感、`<Space>` / `t` / `x` 单独勾选或排除某一条命中、`<Return>` 跳至原文件行、`R`（或 `:w`）调用 `accel_workspace_replace` 批量原子写回磁盘、`q` 关闭。
- **核心职责**：提供可视化的跨项目重构面板（类似 `nvim-spectre` + Emacs `wgrep`），支持在真正落盘前逐行审阅并剔除不想替换的匹配项。

---

## 领域七：状态栏、通知中心与禅模式（3 个扩展） *ui-statusline-zen*

### 29. `lava.org/ext_lualine` — 高信息密度分段状态栏引擎 *ext-lualine*

- **注册入口与服务**：`register_lualine_extension` | 服务键：`SERVICE_LUALINE = "lava.lualine.statusline"`。
- **核心命令与键位**：`lualine-toggle`（`<Space> u l` / `:LualineToggle`）、`lualine-refresh`（`:LualineRefresh`）。
- **核心职责**：按当前模态（Normal / Insert / Visual / Replace / Command / Emacs / Term）动态切换胶囊主题色，分段展示 Git 分支与变更统计、错误/警告诊断徽章、可视选区行列数或多光标计数、宏录制状态、文件编码、语言类型、文档阅读百分比及活跃扩展数。

### 30. `lava.org/ext_noice` — 非侵入式浮动通知卡片与历史查看器 *ext-noice*

- **注册入口与服务**：`register_noice_extension` | 服务键：`SERVICE_NOICE = "lava.noice.notify"`。
- **核心命令与键位**：`notifications` / `noice-history`（`<Space> s n` / `:Notifications` / `:Noice`）、`noice-dismiss`（`<Space> n d` / `:NoiceDismiss`）、`noice-last`（`<Space> n l` / `:NoiceLast`）、`noice-toggle`（`:NoiceToggle`）、`notify`（`:Notify <msg>`）。
- **核心职责**：在窗口右上角渲染自动淡出的非阻塞 Toast 通知卡片，并记录完整的带时间戳通知历史供 `*Notifications*` 缓冲区回溯查阅。

### 31. `lava.org/ext_zen` — 无干扰居中禅模式与上下文聚焦调暗 *ext-zen*

- **注册入口与服务**：`register_zen_extension` | 服务键：`SERVICE_ZEN = "lava.zen.focus"`。
- **核心命令与键位**：`zen-mode` / `zen-mode-toggle`（`<Space> z` / `:ZenMode`）、`twilight` / `twilight-toggle`（`<Space> u t` / `:Twilight`）、`zen-narrow-focus`（`<Space> z n` / `:ZenNarrow`）。
- **核心职责**：`ZenMode` 自动收起侧边辅助装饰并将正文居中固定在舒适阅读列宽；`Twilight` 自动调暗当前代码块以外的周边文本以强化视觉聚焦；`ZenNarrow` 结合缓冲区 Narrowing 实现仅聚焦当前函数或章节的沉浸式写作与编码。

---

## 领域八：语义注释徽章与高亮增强（3 个扩展） *annotations-and-highlighting*

### 32. `lava.org/ext_todo_comments` — 语义待办胶囊徽章与全工作区 `*Todos*` 面板 *ext-todo-comments*

- **注册入口与服务**：`register_todo_comments_extension` | 服务键：`SERVICE_TODO_COMMENTS = "lava.todo_comments.state"`。
- **核心命令与键位**：`todo-next`（`]t`）、`todo-prev`（`[t`）、`todo-quickfix` / `todo-trouble`（`<Space> s t` 打开 `*Todos*` 面板；内部 `<Return>` 跳转、`f` 按标签类别过滤、`r` 刷新、`q` 关闭）、`todo-comments-toggle`。
- **核心职责**：调用 `accel_scan_buffer_todos` 与 `accel_scan_workspace_todos` 识别 `TODO`、`FIXME`、`BUG`、`HACK`、`WARN`、`PERF`、`NOTE` 注释标签，渲染彩色胶囊徽章与行号槽标记，并支持全仓库待办聚合检索。

### 33. `lava.org/ext_colorizer` — 实时真彩色色块预览与六层彩虹括号 *ext-colorizer*

- **注册入口与服务**：`register_colorizer_extension` | 服务键：`SERVICE_COLORIZER = "lava.colorizer.state"`。
- **核心命令与键位**：`colorizer-toggle`（`<Space> u c`）、`rainbow-toggle`（`<Space> u r`）。
- **核心职责**：实时解析代码中的 `#RRGGBB`、`#RGB`、`rgb(...)` 颜色字面量，动态计算明度对比度并以真实背景色渲染前景文字；同时为嵌套括号提供 6 层循环彩虹定界符着色（Rainbow Delimiters）。

### 34. `lava.org/ext_illuminate` — 光标下同名标识符实时高亮与引用跳转 *ext-illuminate*

- **注册入口与服务**：`register_illuminate_extension` | 服务键：`SERVICE_ILLUMINATE = "lava.illuminate.state"`。
- **核心命令与键位**：`illuminate-next-ref`（`]r`）、`illuminate-prev-ref`（`[r`）、`illuminate-toggle`（`<Space> u i`）。
- **核心职责**：当光标停留在任意变量、类型或函数标识符上时，自动在当前视口内高亮所有完整词边界匹配的同名出现位置，并支持通过 `]r` / `[r` 在各引用处间循环跳转。

---

## 领域九：终端、任务编排、Emacs 工具集与会话（4 个扩展） *terminal-tasks-sessions*

### 35. `lava.org/ext_toggleterm` — 多实例原生 POSIX PTY 与 VT100 真彩色终端管理器 *ext-toggleterm*

- **注册入口与服务**：`register_toggleterm_extension` | 服务键：`SERVICE_TOGGLETERM = "lava.toggleterm.manager"`。
- **核心命令与键位**：共 29 个命令；`<C-\>`（支持 `[1..9]<C-\>` 多实例编号切换）、`<Space> t t`（当前窗打开）、`<Space> t f`（浮动终端）、`<Space> t h`（底部分屏终端）、`<Space> t v`（右侧分屏终端）、`<Space> t s`（将当前行或选区发送至终端执行）、`<Space> t r`（运行命令）、`[t` / `]t` 终端切换，`:Term` / `:ToggleTerm` / `:TermSplit` / `:TermVSplit` / `:TermSend`；在 `TERM-INSERT` 中按 `<C-\><C-n>` 切入可自由滚屏复制的 `TERM-NORMAL` 模式（详见 [terminal-and-pty#top]）。
- **核心职责**：基于 `crates/lava-term` 提供零轮询事件驱动的真 POSIX 伪终端，完整支持 `bash`/`zsh`、`htop`、`git`、`cargo` 等全屏与交互式 CLI 应用。

### 36. `lava.org/ext_overseer` — 项目构建/测试任务自动发现与 `*Overseer*` 仪表盘 *ext-overseer*

- **注册入口与服务**：`register_overseer_extension` | 服务键：`SERVICE_OVERSEER = "lava.overseer.tasks"`（开放 `register_template` 自定义任务模板）。
- **核心命令与键位**：`overseer-run`（`<Space> r t` / `:OverseerRun`）、`overseer-rerun`（`<Space> r l` / `:OverseerRerun` 重跑上次任务）、`overseer-toggle` / `overseer`（`<Space> r o` / `:Overseer` 切换任务面板；内部 `<Return>` 选择、`r` 重跑选中项、`q` 关闭）。
- **核心职责**：自动探测工作区内的 `Cargo.toml`、`package.toml`、`Makefile`、`package.json`、`pyproject.toml` 生成构建/测试/检查任务模板，异步执行任务并将编译器报错自动解析注入 Quickfix 列表。

### 37. `lava.org/ext_tools` — Emacs Power Tools（Dired、Occur、后台编译与 `*scratch*` 求值） *ext-tools*

- **注册入口与服务**：`register_tools_extension` | 服务键：`SERVICE_TOOLS = "lava.tools.manager"`。
- **核心命令与键位**：共 25 个经典 Emacs 工具命令——`dired` / `dired-jump`（`<Space> e`；支持 `^`、`+`、`R`、`C`、`x` 批量删除）、`occur`（缓冲区行匹配汇总窗）、`project-search`（并发分片异步项目搜索）、`grep`（`<Space> f g`）、`compile` / `make`、`next-error` / `previous-error`（`:cn` / `:cp`）、`shell-command`（`<M-!>`）、`async-shell-command`（`<M-&>`）、`eval-scratch-line`（`<C-x> <C-e>`）与 `eval-expression`（`<M-:>`）。
- **核心职责**：利用 `spawn_bg_job` 代数效应与后台任务池提供非阻塞编译、Shell 执行、经典 Dired 与实时表达式求值体验。

### 38. `lava.org/ext_persistence` — 启动 `*Dashboard*` 欢迎屏与工作区会话持久化 *ext-persistence*

- **注册入口与服务**：`register_persistence_extension` | 服务键：`SERVICE_PERSISTENCE = "lava.persistence.session"`。
- **核心命令与键位**：`dashboard`（`<Space> s d` / `:Dashboard`；欢迎屏内支持 `f` 找文件、`n` 新建、`g` 全文搜索、`s` 恢复会话、`l` 打开扩展管理、`1..5` 打开最近文件、`q` 退出）、`session-save`（`<Space> q s` / `:SessionSave`）、`session-restore`（`<Space> q l` / `:SessionRestore`）、`session-delete`（`<Space> q d` / `:SessionDelete`）。
- **核心职责**：无文件参数启动 Lava 时呈现清爽的快捷动作与最近文件欢迎页，并支持按项目工作区保存和恢复已打开文件列表、光标位置与窗口布局。

---

## 领域十：全局文档自省与扩展管理（2 个扩展） *introspection-and-manager*

### 39. `lava.org/ext_help` — Emacs Info 超文本阅读器、五域 Apropos 与 Helpful 内省引擎 *ext-help*

- **注册入口与服务**：`register_help_extension` | 服务键：`SERVICE_HELP_STATE = "lava.help.state"`（`HelpState`）。
- **核心命令与键位**：注册 43 个命令与 82 条键位绑定，覆盖 `C-h i` / `:Info`（`*Info*` 超文本手册浏览器）、`C-h d` / `:AproposDocs`（跨手册/命令/选项/扩展/`.ns` 符号的五域全局搜索）、`C-h a` / `C-h o`、`C-h f` / `C-h x`（带 `.ns` 源码预览与 `s` 跳转的函数命令检查器）、`C-h v`（带 `e` 原位实时编辑的 29 项变量检查器）、`C-h m` / `C-h k` / `C-h c` / `C-h w` / `C-h ?` / `<F1>`，以及 Normal `K` / `C-h .` / `:h <topic>` 光标处智能上下文帮助（完整用法见 [emacs-help-and-info#top]）。
- **核心职责**：让整座编辑器的文档手册、运行时状态与底层 Nessa 源码形成彼此互通的超文本知识网络。

### 40. `lava.org/ext_lazy` — 单 VM 扩展管理器与 `:CheckHealth` 运行时诊断面板 *ext-lazy*

- **注册入口与服务**：`register_lazy_extension` | 服务键：`SERVICE_LAZY = "lava.lazy.manager"`。
- **核心命令与键位**：`lazy` / `list-extensions`（`<Space> l` / `:Lazy`）、`:LazySync`、`checkhealth` / `lazy-check-health`（`:CheckHealth` / `*Lazy*` 内按 `c`）；在 `*Lazy*` 面板内支持 `<Return>` / `K` 查看选中扩展详情、`x` / `e` 切换扩展启用状态、`R` 激活全部扩展、`q` / `<Escape>` 关闭。
- **核心职责**：提供直观的可视化扩展状态管理界面，并通过 `:CheckHealth` 一键检验单 Nessa VM、`.nsbc` 字节码缓存、宿主加速原语（`lava-accel`）、POSIX PTY 可用性（`lava-term`）以及全部核心服务的运行健康度。

### 41. `lava.org/ext_lsp` — 全功能 LSP 客户端引擎（跳转、悬停、重命名、代码动作、实时诊断） *ext-lsp*

- **注册入口与服务**：`register_lsp_extension` | 服务键：`SERVICE_LSP_CLIENT = "lava.lsp.client"`。
- **核心命令与键位**：`lsp-definition`（`gd`）、`lsp-references`（`gr`）、`lsp-implementation`（`gI`）、`lsp-type-definition`（`gy`）、`lsp-hover`（`:LspHover` / `<Space> l h`）、`lsp-rename`（`<Space> r n`）、`lsp-code-action`（`<Space> c a`）、`lsp-workspace-symbols`（`<Space> c S`）、`lsp-next-error` / `lsp-prev-error`（`]e` / `[e`）、`:LspInfo`、`:LspStart`、`:LspStop`、`:LspRestart`。
- **核心职责**：驱动 LSP 生命周期同步（`didOpen`/`didChange`/`didSave`/`didClose`）、实时诊断波浪线与虚拟文本推送、`ext_cmp` 实时 LSP 自动补全源、`ext_conform` LSP 格式化对接以及内置工作区符号回退跳转。

### 42. `lava.org/ext_lspconfig` — 18 语言开箱即用 LSP 预设辅助插件与 `*LspConfig*` 面板 *ext-lspconfig*

- **注册入口与服务**：`register_lspconfig_extension` | 服务键：`SERVICE_LSPCONFIG = "lava.lsp.presets"`。
- **核心命令与键位**：`lsp-config` / `lsp-check`（`:LspConfig` / `:LspCheck` / `:Mason` / `<Space> l i`）；在 `*LspConfig*` 仪表盘中按 `<Return>` 查看详情与安装命令、按 `s` 启动当前语言服务器、按 `r` 重新扫描 `$PATH`、按 `q` 关闭。
- **核心职责**：类似 `nvim-lspconfig` + `mason.nvim`，内置 Nessa、Rust（`rust-analyzer`）、Python（`pyright`/`basedpyright`/`ruff`）、TypeScript/JS、C/C++（`clangd`）、Go（`gopls`）、Lua、Zig（`zls`）、Bash、JSON、TOML（`taplo`）、YAML、Markdown（`marksman`）、HTML、CSS、Java（`jdtls`）、Ruby（`ruby-lsp`）、Nix（`nil`/`nixd`）共 18 种语言的根标记探测、本地与远端 SSH 主机 `$PATH` 二进制自动发现、服务端 LSP 开箱自动挂载与安装指引。

### 43. `lava.org/ext_tablist` — Emacs Tab-Line 顶部缓冲区标签栏与快速切换 *ext-tablist*

- **注册入口与服务**：`register_tablist_extension` | 服务键：`SERVICE_TABLIST = "lava.tablist.bar"`。
- **核心命令与键位**：`tablist-next` / `tablist-prev`、`tablist-close`、顶部标签栏鼠标点击切换与关闭。
- **核心职责**：提供干净无粗体的现代缓冲区标签条与标签组导航。

---

## 领域十一：管道式 Diff 引擎、SSH 远程开发与独立服务端集合 *remote-and-diff*

### 44. `lava.org/ext_diff` — 带 stdio `Mapper` 与 `Filter` 管道（`sed`/`awk`/`rg`）的全功能 Diff 插件 *ext-diff*

- **注册入口与服务**：`register_diff_extension` | 服务键：`SERVICE_DIFF = "lava.diff.engine"`（`DiffManager`）。
- **核心命令与键位**：
  - 打开对比：`:DiffThis` / `:DiffBuffers`（`<Space> d b`）、`:DiffFiles <left> <right>`（`<Space> d f`）、`:DiffGit [ref]`（`<Space> d g` 工作区 vs `HEAD`）、`:DiffSaved`（`<Space> d s` 未保存缓冲区 vs 磁盘/远端文件）、`:DiffDir <left> <right>`（`<Space> d d` 目录级递归树对比）、`:DiffHistory`（`<Space> d h` Git 提交历史浏览器）、`:DiffClose`（`<Space> d q`）。
  - **stdio 管道变换与过滤**：
    - **`Filter`（行级过滤器，`:DiffFilter [--left|--right] <cmd>` / Diff 视图按 `f`）**：将完整文本通过 `stdin` 送入外部命令（如 `rg -v '^DEBUG'`、`rg 'ERROR|WARN'`、`awk 'NF > 0'`），仅保留输出行参与对比，同时追踪真实原始行号（且将 `rg`/`grep` 无匹配退出码 `1` 正常视为空匹配集而非报错）。
    - **`Mapper`（1:1 行归一化映射器，`:DiffMap [--left|--right] <cmd>` / Diff 视图按 `m`）**：将每行通过 `stdin` 送入 `sed` / `awk`（如 `sed -E 's/[0-9]{4}-[0-9]{2}-[0-9]{2}/<DATE>/g'`、`awk '{$1=""; print}'`）做归一化后再计算差异，同时保留原始未修改行内容与原始行号，按 `v` 可在 `[VIEW: MAPPED]` 与 `[VIEW: ORIGINAL]` 之间实时无损切换。
    - 多级流水线管理：`:DiffPreset`（`P` 内置去时间戳、去 UUID/Hex 地址、忽略空白、`rg` 仅看报错、去注释等预设）、`:DiffPipeline`（`L` 查看/启停/删除单级规则）、`:DiffClear`（`C` 清空全部管道并重算）。
  - 视图与补丁操作：双栏同步滚动分屏（Side-by-Side）与单栏 Unified 视图切换（`t`）、行内字符/词级精细高亮、`w` 忽略空白切换、`c` 折叠上下文行数调整、`[c` / `]c` Hunk 跳转、`do`（Diff Obtain）与 `dp`（Diff Put）双向 Hunk 应用并支持完整撤销树。

### 45. `lava.org/ext_remote` — 全透明 SSH Remote 开发、远程浏览器、隧道与服务端 LSP 协同 *ext-remote*

- **注册入口与服务**：`register_remote_extension` | 服务键：`SERVICE_REMOTE = "lava.remote.manager"`（`RemoteExtension`）。
- **核心命令与键位**：`:RemoteConnect [user@host[:port]]`（`<Space> S c`）、`:RemoteStatus` / `:RemoteInfo`（`<Space> S i` 打开 `*Remote*` 控制面板）、`:RemoteDisconnect`（`<Space> S d`）、`:RemoteBrowse [ssh://host/path]`（`<Space> S b` Ranger 键位远程目录浏览器）、`:RemoteFiles`（`<Space> S f` 远端全项目模糊找文件）、`:RemoteGrep`（`<Space> S g` 远端 `rg` 实时全文检索并跳转）、`:RemoteShell`（`<Space> S s` 在原生 POSIX PTY 中直连远端目录交互式 SSH Shell）、`:RemotePortForward <[bind:]local:remote_host:remote_port>`（`<Space> S p` SSH `-L`/`-R` 隧道管理）、`:RemoteDeploy`（`<Space> S D` 部署/探测远端 `lava-server`）、`:RemoteExec <cmd>`（`<Space> S !`）。
- **核心职责**：打通 `ext_document` 虚拟文件系统（`:e ssh://host/path`、`/ssh:host:/path`、`C-x C-f` 远端主机与路径补全、`:w` 远端原子保存）与服务端 LSP（如远端主机上的 `clangd`、`rust-analyzer`、`pyright`、`gopls`），无需拉回整仓即可获得远端头文件跨文件 `gd` 跳转、悬停文档、实时诊断与补全。

---

## 独立无头服务端集合：`server/`（`lava.server/*` 与 `lava-server` 二进制） *lava-server-collection*

为了让远程服务器无需安装 GTK4/图形库即可提供低延迟 RPC 与服务端 LSP 托管，仓库根目录下的 `server/` 提供了一套**完全独立的 Nessa Collection**，由 `crates/lava-server` 编译为单文件无头二进制 `lava-server`（支持 `lava-server --stdio`、`lava-server --lsp-proxy <cwd> <cmd> [args...]`、`lava-server list-extensions`、`lava-server check`）：

| 服务端包 / 扩展 ID | 源码路径 | 核心职责与暴露的 JSON-RPC 方法 |
| :--- | :--- | :--- |
| `lava.server/server_sys` | `server/packages/server_sys` | 服务端无图形宿主原语绑定（文件、进程、LSP 桥与 `Content-Length` RPC 帧收发）。 |
| `lava.server/server_core` | `server/packages/server_core` | 服务端微内核 `ServerKernel`、JSON-RPC 2.0 路由器与方法注册表（`server/ping`、`server/capabilities`、`server/shutdown`）。 |
| `lava.server/srv_fs` | `server/extensions/srv_fs` | 远端原子文件系统服务：`fs/stat`、`fs/read`、`fs/write`（临时文件原子替换）、`fs/readdir`、`fs/mkdir`、`fs/remove`、`fs/rename`、`fs/copy`、`fs/realpath`、`fs/find_root`。 |
| `lava.server/srv_search` | `server/extensions/srv_search` | 远端高速检索服务：`search/files`（优先 `rg --files`）、`search/grep`（优先 `rg --vimgrep`）、`search/symbols`。 |
| `lava.server/srv_git` | `server/extensions/srv_git` | 远端 Git 仓库服务：`git/status`、`git/head_lines`、`git/diff`、`git/blame`、`git/log`。 |
| `lava.server/srv_exec` | `server/extensions/srv_exec` | 远端命令与环境探测：`exec/run`（支持 `cwd` 与 `stdin` 注入）、`exec/which`、`exec/sysinfo`。 |
| `lava.server/srv_lsp` | `server/extensions/srv_lsp` | 服务端 LSP 守护进程管理器（在远端直接运行 `clangd`、`rust-analyzer` 等）：`lsp/which`、`lsp/find_root`、`lsp/spawn`、`lsp/stop`、`lsp/status`、`lsp/did_open`、`lsp/did_change`、`lsp/did_save`、`lsp/did_close`、`lsp/request`、`lsp/poll_events`。 |
| `lava.server/srv_app` | `server/extensions/srv_app` | 顶层无头服务端编排器：拓扑加载全部 6 个 `srv_*` 扩展并驱动 `Content-Length` JSON-RPC 2.0 主循环。 |
