# Emacs 全局帮助、Info 超文本手册与自省系统 *emacs-help*

Lava 秉承 Emacs **“自文档化、可实时内省（Self-Documenting & Live-Introspectable）”** 的核心哲学，通过内置扩展 `lava.org/ext_help`（服务键 `"lava.help.state"`）、Rust 多核文档与符号索引器 `lava-accel` 以及模糊检索面板 `lava.org/ext_telescope`，将 **Info 超文本手册阅读器（`*Info*`）**、**五域全局文档搜索引擎（`*Apropos*`）**、**`helpful` 风格运行时与源码内省面板（`*Help*`）** 以及 **Telescope 实时预览文档检索器** 融为一体。

无论你正在查询某个快捷键、实时切换编辑器选项、通读 Lava 与 Nessa 语言规范，还是直接跳转到某个内置命令背后的 `.ns` 源码定义行，都可以在毫秒级内完成，无需离开键盘流。

相关手册请参阅：[index#top]、[vim-and-emacs-keybindings#top]、[minibuffer-and-completion#top]、[plugins-catalog#top]、[extension-development#top] 与 [nessa-for-lava#top]。

---

## 自省架构与三大专用缓冲区 *help-architecture*

Lava 的帮助系统由编译期静态链接进单 Nessa VM 的三大层协同驱动：

1. **原生多核索引加速层（`crates/lava-accel` / `lava.org/lava_accel`）**：
   - `accel_index_docs(doc_roots)`：并行扫描所有文档根目录下的 Markdown 手册，解析 `#` / `##` / `###` 标题树、提取尾部 `*help-tag*` 锚点，并构建具有 `Next` / `Prev` / `Up` / `Top` 拓扑关系的 `DocNode` 超文本节点图。
   - `accel_search_docs(doc_roots, query, limit)`：支持 Orderless 多分词与 `!排除词` 语法的全量文档全文搜索引擎，对标题、标签与正文段落进行加权打分并返回精确上下文摘要与行号。
   - `accel_find_symbol_defs(code_roots, query, limit)`：扫描所有 `.ns` 源码包，定位 `fn`、`struct`、`enum`、`trait`、`effect`、`type` 及 `register_command(...)` 的源文件、行号、签名与上方紧邻的 `--` 文档注释。
2. **三大专用交互缓冲区（Special Buffers）**：
   - `*Info*`（`major_mode = "info"`）：树状结构化超文本手册阅读器，维护独立的节点前进/后退历史栈与光标记忆。
   - `*Apropos*`（`major_mode = "apropos"`）：跨手册、命令、选项、扩展与源码符号的全局聚合检索报表。
   - `*Help*`（`major_mode = "help"`）：对单个命令、选项、按键、主模式、扩展或 Nessa 符号的深度内省页面，内嵌源码预览、可点击交叉引用链接与实时变量编辑。
3. **默认索引根目录（Doc Roots & Code Roots）**：
   - **文档根目录（`resolve_doc_roots`）**：自动发现当前工作区 `<cwd>/docs`、Lava 官方手册目录 `/home/fin/apps/lava/docs`、Nessa 语言规范目录 `/home/fin/nessa2/docs`（在 Info 中以 `nessa:...` 命名空间呈现），以及用户通过 `HelpState.extra_doc_roots` 追加的自定义文档路径。
   - **源码根目录（`resolve_code_roots`）**：自动覆盖 `<cwd>/packages`、`<cwd>/extensions`、`<cwd>/src`、Lava 核心包 `/home/fin/apps/lava/packages`、全部 40 个内置扩展 `/home/fin/apps/lava/extensions`、Nessa 标准库 `/home/fin/nessa2/library/std` 以及 `HelpState.extra_code_roots`。

---

## 全局帮助入口快捷键总表 *help-entrypoints*

Lava 同时提供正统 **Emacs `C-h` 前缀序列**、**Evil-Vim `<Space> h` Leader 前缀序列**、**Normal 模式 `K`** 以及 **`:Ex` 命令行入口**，并支持在 `:Ex` 与 Minibuffer 中通过 Vertico + Orderless 实时补全主题名（详见 [minibuffer-and-completion#top]）：

| Emacs 快捷键 | Evil / Leader 快捷键 | Ex 命令 / `M-x` 命令名 | 目标缓冲区 | 核心功能说明 |
| :--- | :--- | :--- | :--- | :--- |
| `C-h i` | `<Space> h i` | `:Info [topic]` / `info` | `*Info*` | 打开超文本手册浏览器（无参时恢复上次节点或打开 `(dir)Top`） |
| `C-h d` | `<Space> h d` / `<Space> h h` | `:AproposDocs <q>` / `apropos-documentation` | `*Apropos*` | **五域全局文档搜索**（跨手册、命令、选项、扩展服务与 `.ns` 符号） |
| `C-h a` | `<Space> h a` | `:AproposCommand <q>` / `apropos-command` | `*Apropos*` | 按 Orderless 分词检索所有交互式与 Ex 命令、快捷键及摘要 |
| `C-h o` | `<Space> h o` | `:Apropos <q>` / `apropos` | `*Apropos*` | 统一符号、命令、编辑器变量、扩展与 `*help-tag*` 检索 |
| `C-h f` | `<Space> h f` | `describe-function` | `*Help*` | 查看命令或 Nessa 函数的文档、全部按键绑定及 10 行 `.ns` 源码预览 |
| `C-h x` | `<Space> h x` | `describe-command` | `*Help*` | 查看交互式命令详情与 `.ns` 注册源码位置 |
| `C-h v` | `<Space> h v` | `describe-variable` | `*Help*` | 查看 29 项 `EditorOptions` 的当前值、默认值、`:set` 语法并按 `e` 实时修改 |
| `C-h m` | `<Space> h m` | `describe-mode` | `*Help*` | 查看当前缓冲区主模式、局部按键表、已激活 Hook 与全部加载扩展 |
| `C-h k` | `<Space> h k` | `describe-key` | `*Help*` | 按键序列深度检查器：解析任意按键串绑定的命令、所属扩展与源码 |
| `C-h c` | `<Space> h c` | `describe-key-briefly` | Echo 区 | 在底部状态栏单行简明回显某组按键绑定的命令或子前缀数量 |
| `C-h w` | `<Space> h w` | `where-is` | Echo 区 | 反向查询某个命令绑定在哪些 Keymap 的哪些快捷键及 `:Ex` 调用方式 |
| `C-h b` | `<Space> h b` | `describe-bindings` | `*Help*` | 列出当前所有全局、模式与局部 Keymap 的完整按键映射参考表 |
| `C-h p` | `<Space> h p` | `describe-extensions` | `*Help*` | 检查单 VM 内核中的全部已注册扩展、依赖拓扑与已发布服务 |
| `C-h e` | — | `view-echo-area-messages` | `*Messages*` | 打开 `*Messages*` 运行时日志缓冲区检查历史状态消息 |
| `C-h .` | Normal `K` / `<Space> h .` | `:h [topic]` / `:help [topic]` / `helpful-at-point` | `*Info*` / `*Help*` | **光标处智能上下文帮助**（自动识别标签、命令、选项、`.ns` 符号） |
| `C-h ?` / `C-h C-h` / `<F1>` | `<F1>` / `<Space> h ?` | `help-for-help` | `*Help*` | 打开帮助系统的总导览速查面板（含手册快速跳转链接） |
| — | `<Space> f h` | `:Telescope help_tags` / `telescope-help-tags` | Telescope | 浮动双栏模糊检索所有手册章节、`*help-tag*` 与命令（实时预览） |
| — | `<Space> s H` / `<Space> h s` | `:Telescope docs` / `telescope-docs` | Telescope | 浮动双栏实时 Orderless 全文检索 Lava 与 Nessa 手册（`<C-q>` 导出） |

---

## Info 超文本手册浏览器 *emacs-info*

按下 `C-h i`、`<Space> h i` 或执行 `:Info` 即可进入 `*Info*` 超文本阅读器（`major_mode = "info"`）。如果你曾阅读过某个章节，无参调用 `C-h i` 会自动恢复上次阅读的节点与光标行；否则打开顶层总目录节点 `(dir)Top`。

### 顶层目录与节点命名体系 *info-nodes*

`lava-accel::index_docs` 将每个 Markdown 文件编译为一棵多级 Info 节点树：

- **顶层总目录 `(dir)Top`**：自动聚合两套核心文档库——**Lava Editor Manuals**（如 `index#top`、`architecture#top`、`user-guide#top` 等）与 **Nessa Language Manuals**（以 `nessa:` 前缀标识，如 `nessa:grammar/overview#top`、`nessa:algebraic-effect#top`、`nessa:multi-prompt-delimited-continuation#top`）。
- **手册根节点 `<manual>#top`**：每篇文档的第一个 `#` 一级标题对应 `<manual>#top`（例如 `dirvish-and-ranger#top`、`terminal-and-pty#top`）。
- **章节子节点 `<manual>#<slug>`**：后续 `##` 与 `###` 标题生成同文件内的层级子节点，自动维护父子菜单 `* Menu:` 以及相邻同级节点的 `Next`、`Prev`、`Up` 与 `Dir: [(dir)Top]` 导航头。

每个 `*Info*` 页面顶部均带有标准面包屑导航栏，例如：

```text
File: emacs-help-and-info,  Node: emacs-help-and-info#top,  Next: [...],  Up: [(dir)Top],  Dir: [(dir)Top]
```

### 超文本交叉引用链接格式 *info-hyperlinks*

在 `*Info*` 与 `*Help*` 缓冲区中，链接扫描器会自动识别以下所有方括号超链接语法；可通过 `<Tab>` / `<S-Tab>` 在链接间快速跳转，按 `<Return>` 立即打开目标：

- `[manual#top]` / `[manual#slug]` / `[nessa:topic#top]` / `[(dir)Top]`：跳转到指定 Info 手册或章节节点。
- `[Info: <node-or-tag>]`：按节点 ID 或 `*help-tag*` 打开对应 Info 章节并精确定位到锚点所在行。
- `[Source: /path/to/file.ns:128]`：直接打开对应源码文件并将光标定位到第 128 行。
- `[Command: describe-variable]`：在 `*Help*` 中打开该命令的内省详情页。
- `[Variable: tab_width]`：在 `*Help*` 中打开该编辑器选项的内省与实时编辑页。
- `[Extension: lava.org/ext_help]`：在 `*Help*` 中打开该扩展的依赖、服务、命令与按键全景页。
- `[Symbol: SystemKernel]`：在 `*Help*` 中打开该 Nessa 类型或函数的签名与源码页。
- 标准相对路径 Markdown 链接 `[文本](./user-guide.md)`：自动解析相对路径并在 `*Info*` 中打开对应手册节点（若目标非索引文档则直接作为文件打开）。

### `*Info*` 模式完整快捷键表 *info-keymap*

在 `*Info*`（`mode:info`）缓冲区中，除常规滚动与光标移动外，激活以下专用导航键：

| 快捷键 | 绑定命令 | 行为说明 |
| :--- | :--- | :--- |
| `<Return>` | `info-follow-at-point` | 激活当前行上的 `[manual#slug]`、`[Source: ...]`、`* Menu:` 条目或 Markdown 链接 |
| `<Tab>` | `info-next-link` | 光标跳至缓冲区内下一个超文本链接 |
| `<S-Tab>` / `<BackTab>` | `info-prev-link` | 光标跳至缓冲区内上一个超文本链接 |
| `n` | `info-next-node` | 前往线性阅读顺序中的下一个（`Next`）Info 节点 |
| `p` | `info-prev-node` | 返回线性阅读顺序中的上一个（`Prev`）Info 节点 |
| `u` / `^` | `info-up-node` | 向上返回父级章节节点（根节点上按 `u` 返回 `(dir)Top`） |
| `t` | `info-top-node` | 回到当前手册的 `Top` 根节点（`<manual>#top`） |
| `d` | `info-directory` | 直接回到全局手册总目录 `(dir)Top` |
| `l` / `H` / `<C-o>` | `info-history-back` | 在 `*Info*` 浏览历史栈中后退（精确恢复之前的节点与光标行） |
| `r` / `L` | `info-history-forward` | 在 `*Info*` 浏览历史栈中前进 |
| `g` | `info-goto-node` | 弹出带 Orderless 补全的 Minibuffer，选择并跳转到任意 Info 节点 |
| `m` | `info-menu` | 弹出 Minibuffer 选择当前节点的子章节菜单项（`* Menu:`） |
| `i` | `info-index` | **全局索引查找**：在所有 `*help-tag*`、29 个选项与全部交互命令中补全跳转 |
| `s` | `info-search` | 在所有已索引手册中执行全文搜索，自动跳至首个匹配处并开启搜索高亮 |
| `]` / `[` | `info-search-next` / `info-search-prev` | 在最近一次 `s` 全文搜索的命中结果间循环前/后跳转 |
| `?` | `help-for-help` | 打开帮助系统快捷键速查表 |
| `q` | `info-quit` | 关闭或离开 `*Info*`，自动切回进入帮助系统前的编辑缓冲区 |

---

## 全局文档搜索与 Apropos 系统 *emacs-apropos*

当你不确定某个概念位于哪本手册、叫什么命令名、由哪个扩展提供或定义在哪个 `.ns` 文件中时，Lava 的 `*Apropos*`（`major_mode = "apropos"`）提供跨运行态与静态文档的聚合搜索。所有查询均原生支持 **Orderless 空格多词乱序匹配** 与 **`!排除词` 过滤**（详见 [minibuffer-and-completion#top]）。

### 五域全局文档搜索（`C-h d` / `:AproposDocs`） *apropos-docs*

按下 `C-h d`、`<Space> h d`（或 `<Space> h h`），或执行 `:AproposDocs <query>`（也可在 `:h ?<query>` 中以问号前缀触发），将运行 `apropos-documentation`，一次性并行检索以下 **5 大知识域** 并将结果分层呈现在 `*Apropos*` 缓冲区中：

1. **Manuals & Info Nodes（`[DOC]`）**：调用 `accel_search_docs` 检索 Lava 全部手册与 Nessa 语言文档的标题、`*help-tag*` 与正文段落，展示节点 ID、命中标签与匹配行上下文摘要。
2. **Interactive Commands & Keybindings（`[CMD]`）**：遍历内核注册的全部交互式命令与 `:Ex` 命令，同时匹配命令名、所属扩展 ID、当前绑定的快捷键序列与功能摘要。
3. **Editor Variables / Options（`[VAR]`）**：检索 `EditorOptions` 的全部 **29 个配置项**（同时匹配全名、`:set` 短别名、类型、当前值以及中英文双语说明）。
4. **Extensions, Services & Hooks（`[EXT]`）**：检索全部 **40 个内置扩展** 的名称、功能描述、已发布的 `SERVICE_*` 服务键以及已订阅的内核 Hook 主题。
5. **Nessa & Lava Source Symbols（`[SYM]`）**：调用 `accel_find_symbol_defs` 扫描 `packages/*`、`extensions/*` 与 Nessa `library/std` 中的函数、结构体、枚举、代数效应与类型定义及其 `--` 文档注释，附带精确的 `文件名:行号`。

### 命令与统一符号检索（`C-h a` 与 `C-h o`） *apropos-variants*

除 `C-h d` 外，Lava 还提供两个聚焦型 Apropos 入口：

- **`C-h a` / `<Space> h a` / `:AproposCommand <query>`（`apropos-command`）**：仅聚焦于第 2 域（全部交互式命令与 Ex 命令），适合寻找“保存窗口”“切换终端”“格式化”对应的具体命令与按键（例如输入 `term split` 或 `git !conflict`）。
- **`C-h o` / `<Space> h o` / `:Apropos <query>`（`apropos`）**：统一符号、命令、选项、扩展与文档标签的综合检索视图。

### `*Apropos*` 模式完整快捷键表 *apropos-keymap*

在 `*Apropos*`（`mode:apropos`）报表缓冲区中，每一条 `[DOC]`、`[CMD]`、`[VAR]`、`[EXT]`、`[SYM]` 条目都是可直接交互的活动对象：

| 快捷键 | 绑定命令 | 行为说明 |
| :--- | :--- | :--- |
| `<Return>` | `apropos-open-at-point` | 打开当前条目的深度视图：`[DOC]` 进入 `*Info*` 对应行，其余四类在 `*Help*` 打开内省页 |
| `s` / `o` / `gd` | `apropos-goto-source` | **一键直达源码**：直接打开该文档 `.md` 或命令/变量/扩展/符号所在的 `.ns` 源文件精确定义行 |
| `<Tab>` / `n` | `apropos-next-item` | 光标跳至下一条 Apropos 命中结果并在状态栏回显条目摘要 |
| `<S-Tab>` / `p` | `apropos-prev-item` | 光标跳至上一条 Apropos 命中结果 |
| `g` / `R` | `apropos-refresh` | 以当前查询词为初始值重新弹出提示符，发起新的 Apropos 检索 |
| `q` | `apropos-quit` | 退出 `*Apropos*` 并恢复原编辑缓冲区 |

---

## Helpful 深度运行时与源码内省 *helpful*

受 Emacs 经典插件 `helpful` 启发，Lava 的 `*Help*` 缓冲区（`major_mode = "help"`）不仅展示静态说明文字，还直接从正在运行的单 Nessa VM（`SystemKernel` + `EditorWorld`）读取实时状态，并自动关联 `.ns` 源码片段。

### 函数与命令内省（`C-h f` / `C-h x`） *describe-function*

按下 `C-h f`（`describe-function`）或 `C-h x`（`describe-command`），通过带 Marginalia 键位提示的 Minibuffer 选择任一命令或 Nessa 符号后，`*Help*` 页面将完整展示：

- 命令名称、交互属性（Interactive / Ex-only）与所属扩展链接（如 `[Extension: lava.org/ext_toggleterm]`）。
- 当前在所有 Keymap（`vim:normal`、`vim:leader`、`emacs:global`、`mode:*` 等）上的实时快捷键绑定列表与 `:Ex` 调用形式。
- 源码定位链接 `[Source: /home/fin/apps/lava/extensions/.../lib.ns:LINE]`（按 `s` 或 `gd` 直接跳转编辑）以及从源文件中实时截取的 **10 行带行号 `.ns` 实现代码预览**。

### 编辑器变量内省与实时编辑（`C-h v`） *describe-variable*

按下 `C-h v`（`describe-variable`）可检查 `EditorOptions` 的全部 **29 项配置**（详见 [user-guide#top]）：

- 展示变量名、`:set` 短别名（如 `rnu`、`ts`、`so`）、值类型（`bool` / `i64` / `String`）、**当前实时值**、**出厂默认值**（若已修改会醒目标注 `modified`）以及中英文详细语义说明。
- **原位实时修改（按 `e`）**：在变量的 `*Help*` 页面中直接按 `e`（或在提示行按 `<Return>`）：
  - 若为 `bool` 选项（如 `relativenumber`、`wrap`、`hlsearch`），按 `e` **立即翻转布尔值**、通知渲染管线并原位刷新 `*Help*` 页面！
  - 若为 `i64` 或 `String` 选项（如 `tab_width`、`scrolloff`、`theme_name`），自动弹出预填当前值的输入框，校验生效后立即更新编辑器与当前 `*Help*` 视图。

### 模式、按键、扩展与日志内省（`C-h m/k/c/w/b/p/e`） *describe-runtime*

- **`C-h m`（`describe-mode`）**：展示当前缓冲区的 `major_mode`、文件路径、当前模态状态、`mode:<major_mode>` 局部按键覆盖表（每条命令均为可点击 `[Command: ...]` 链接）、内核当前激活的全部 Hook 主题与监听器计数，以及全部已加载扩展列表。
- **`C-h k`（`describe-key`）与 `C-h c`（`describe-key-briefly`）**：检查任意按键序列（如 `<C-x> <C-f>`、`<Space> f h`、`g c c`）在不同 Keymap 中绑定的目标命令；`C-h k` 打开完整 `*Help*` 页，`C-h c` 在底部状态栏输出单行摘要（若输入的是前缀键如 `<C-x>`，还会提示其下辖子绑定数量）。
- **`C-h w`（`where-is`）**：输入命令名，立即在状态栏与 `*Messages*` 中列出触发该命令的所有快捷键与 `:Ex` 形式。
- **`C-h b`（`describe-bindings`）与 `C-h p`（`describe-extensions`）**：全景列出当前 VM 内所有按键表或所有扩展、服务（`kernel.services`）及依赖关系。
- **`C-h e`（`view-echo-area-messages`）**：切换到 `*Messages*` 缓冲区查看历史回显与诊断记录。

### 光标处智能上下文帮助（`K` / `C-h .` / `:h`） *helpful-at-point*

在编辑 `.ns` 配置文件（如 `init.ns`）、扩展源码或阅读 Markdown 文档时，将光标放在任意标识符上按下 Normal 模式的 **`K`**（或全局 **`C-h .`**、`<Space> h .`），或者直接输入 **`:h <topic>`** / **`:help <topic>`**，`helpful-at-point` 会提取光标下的符号词元（包含字母、数字、`-`、`_`、`.`、`:`、`*`、`/`），按以下确定性优先级自动解析并打开最匹配的帮助视图：

1. **`?<query>` 前缀**：若输入以 `?` 开头（例如 `:h ?orderless` 或 `:h ?pty`），直接调用五域全局文档搜索 `apropos-documentation`（`*Apropos*`）。
2. **`*help-tag*` 锚点或 `manual#node`**：若词元被 `*...*` 包裹（如文档中的 `*ranger-keys*`）或使用 `:h <topic>` 语法，优先精确匹配 Info 节点 ID、`*help-tag*`（自动滚动到锚点所在行）或手册文件名（如 `:h dirvish-and-ranger`、`:h *nsbc-cache*`）。
3. **已注册命令名**：若精确匹配内核命令（如 `split-window-right`、`toggleterm`），在 `*Help*` 打开该命令的函数与源码内省页。
4. **`EditorOptions` 变量名或短别名**：若匹配 29 项编辑器选项之一（如 `tab_width`、`rnu`、`colorscheme`），在 `*Help*` 打开可按 `e` 实时修改的变量内省页。
5. **Nessa / Lava 源码符号定义**：通过 `accel_find_symbol_defs` 精确匹配 `SystemKernel`、`PromptRequest`、`Buffer`、`ask_prompt` 等 `.ns` 符号，在 `*Help*` 展示签名、文档注释与源码预览。
6. **全文文档与模糊符号兜底**：若无精确同名项，自动回退到 `accel_search_docs` 全文最相关章节并在 `*Info*` 打开对应行，最后兜底模糊符号匹配。

### `*Help*` 模式完整快捷键表 *help-keymap*

在 `*Help*`（`mode:help`）缓冲区中支持完整的超文本前进/后退历史栈与一键跳源码操作：

| 快捷键 | 绑定命令 | 行为说明 |
| :--- | :--- | :--- |
| `<Return>` | `help-follow-at-point` | 激活当前行上的 `[Command: ...]`、`[Extension: ...]`、`[Source: ...]`、`[Info: ...]` 链接 |
| `s` / `gd` | `help-goto-source` | 直接打开当前内省目标对应的 `.ns` 源文件并定位到定义行 |
| `e` | `help-edit-variable` | 当查看 `C-h v` 变量页时，原位翻转布尔值或弹出提示框修改数值/字符串选项 |
| `i` | `info` | 直接切换到 `*Info*` 超文本手册阅读器 |
| `<Tab>` | `help-next-link` | 跳至 `*Help*` 页内下一个可点击链接 |
| `<S-Tab>` / `<BackTab>` | `help-prev-link` | 跳至 `*Help*` 页内上一个可点击链接 |
| `l` / `H` / `<C-o>` | `help-history-back` | 在 `*Help*` 访问过的页面历史栈中后退（保留光标行） |
| `r` / `L` | `help-history-forward` | 在 `*Help*` 页面历史栈中前进 |
| `q` | `help-quit` | 关闭或离开 `*Help*`，恢复进入帮助前的活跃编辑缓冲区 |

---

## Telescope 交互式帮助与全文预览选择器 *telescope-help*

如果你偏爱边输入边实时查看右侧浮动预览窗的交互体验，`lava.org/ext_telescope` 与 `ext_help` / `lava_accel` 深度集成了四款专用于帮助与自省的 Picker：

1. **帮助标签与章节选择器（`<Space> f h` / `:Telescope help_tags`）**：
   - 聚合全部 Lava 与 Nessa 手册章节标题、所有 `*help-tag*` 锚点以及全部交互式命令。
   - 左侧支持 Orderless 多词模糊过滤，右侧浮动窗实时高亮预览目标 Markdown 手册对应段落；按 `<Return>` 自动通过 `info` 打开目标 Info 节点（若选中的是命令则自动调用 `describe-function` 打开 `*Help*`）。
2. **实时全局文档全文检索（`<Space> s H` / `<Space> h s` / `:Telescope docs`）**：
   - 每敲击一个字符即实时调用原生多核 `accel_search_docs` 在所有 Lava 与 Nessa 文档中执行 Orderless 全文检索（例如输入 `continuation prompt` 或 `pty !windows`）。
   - 右侧预览窗即时定位并高亮命中行上下文；按 `<Return>` 在 `*Info*` 打开对应章节，按 **`<C-q>`** 可将当前过滤出的全部文档命中项一键导出至 **Quickfix 列表**（`:cnext` / `:cprev` 逐条研读）。
3. **命令面板与快捷键检索器（`<Space> f C` / `:Telescope commands` 与 `<Space> s k` / `:Telescope keymaps`）**：
   - `commands` 提供全量命令的 Orderless 面板检索与直接执行；`keymaps` 允许按按键名（如 `<C-x>`、`<Space> g`）或描述词实时反查所有扩展注册的键位映射。

---

## 编写可被 Info 与 Apropos 索引的文档规范 *authoring-help-docs*

无论你是在编写自定义扩展文档，还是通过 `~/.config/lava/init.ns` 向 `HelpState` 注册团队内部知识库，只需遵循以下三条规则，即可自动获得 `*Info*` 节点树、`:h` 补全与 `C-h d` 全文检索支持：

1. **为每个标题添加尾部 `*help-tag*` 锚点**：
   - 在 `#`、`##`、`###` 标题末尾附带由单星号包裹的小写 ASCII 字母、数字或连字符组成的唯一标签（严禁在星号内部包含空格），例如：
     ```markdown
     # 自定义构建流水线手册 *custom-pipeline*
     ## 增量缓存配置项 *pipeline-cache-config*
     ```
   - `lava-accel::index_docs` 会剥离尾部的 `*tag*` 作为干净的展示标题，同时将该标签写入 `DocNode.tags` 供 `:h *pipeline-cache-config*`、`C-h i` -> `i` 与 `:Telescope help_tags` 精确跳转。
2. **使用 `[manual#top]` 与 `[Info: *tag*]` 建立超文本互链**：
   - 跨手册引用主入口使用 `[user-guide#top]`、`[extension-development#top]`；精确指向特定小节锚点可使用 `[Info: *services-and-hooks*]` 或同目录下的相对 Markdown 链接。
3. **在 `init.ns` 中动态挂载自定义文档与源码目录**：
   - 通过内核服务 `"lava.help.state"` 获取 `HelpState` 实例后，可随时追加自定义目录：
     ```nessa
     use lava.org/ext_help.{SERVICE_HELP_STATE, HelpState}

     let help_opt = kernel.service(SERVICE_HELP_STATE)
     if help_opt != null {
         let hs = help_opt.as(HelpState)
         hs.extra_doc_roots.push("/path/to/my_team_docs")
         hs.extra_code_roots.push("/path/to/my_custom_extensions")
     }
     ```
