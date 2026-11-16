# Lava 手册总目录与导读 *lava-index* *help-top*

欢迎来到 **Lava 可编程现代编程环境** 官方文档中心！本手册是 Emacs `*Info*` 超文本浏览器（按 `C-h i` 或执行 `:Info` 打开 `(dir)Top`）与全局文档搜索引擎 `apropos-documentation`（按 `C-h d` 或执行 `:AproposDocs`）的总入口节点。

在 `*Info*` 缓冲区中阅读本页时，您可以直接按 `<Tab>` / `<S-Tab>` 在形如 `[manual#slug]` 的超文本节点引用之间跳转，并按 `<Return>` 深入任意专题章节；按 `l` 返回上一历史节点，按 `u` 回到上级目录，按 `s` 搜索当前手册，或按 `q` 退出阅读器。

---

## 按角色推荐的阅读路线图 *reading-roadmap*

无论您是首次启动 Lava 的日常使用者、追求极致效率的 Vim/Emacs 重度用户，还是希望用 Nessa 语言改造编辑器的扩展开发者，都可以按以下路径循序渐进：

### 新手 10 分钟上手路线 *roadmap-beginner*

1. **先读日常入门**：打开 `[user-guide#top]`，掌握 `lava` 命令行参数、首次启动后的前 5 分钟操作、分屏/标签页管理以及内置 8 套配色切换。
2. **掌握交互中枢**：阅读 `[minibuffer-and-completion#top]`，学会用空格多词乱序匹配（Orderless）与 `!排除` 语法在 `M-x`、`C-x C-f` 和 `:` 提示符中瞬时定位任意目标。
3. **随时自救查询**：浏览 `[emacs-help-and-info#top]`，记住 `C-h ?`（帮助总览）、`C-h k`（按键查询）、`C-h f`（命令文档与源码跳转）和 `C-h d`（全局全文搜索）——从此无需离开编辑器即可查阅一切细节。

### Vim 与 Emacs 效率专家路线 *roadmap-power-user*

1. **精通双模键位与文本对象**：查阅 `[vim-and-emacs-keybindings#top]`，全面了解 Evil Normal/Visual-Block/Operator-Pending、分叉 `UndoTree`（`g-`/`g+`）、宏录制、`C-u` 通用前缀参数、`C-x` 窄化/矩形命令，以及 `<Space>` Leader 快捷键矩阵。
2. **高效文件与终端工作流**：阅读 `[dirvish-and-ranger#top]` 掌握三栏 Miller Columns 与 Ranger 批量文件操作；阅读 `[terminal-and-pty#top]` 掌握 `<C-\>` 悬浮/分屏 POSIX PTY 终端与 `<C-\><C-n>` 滚回复制模式。
3. **发掘全部内置能力**：翻阅 `[plugins-catalog#top]`，一览开箱即用的 **40 个内置 Extension**（涵盖 Magit、Gitsigns、Telescope、Harpoon、Spectre、Trouble、Aerial、Conform、Cmp、Snippets、Zen、Overseer、Lazy 等）。

### 架构师与插件开发者路线 *roadmap-developer*

1. **理解单 VM 与无锁渲染底座**：深入 `[architecture#top]`，剖析 8 大核心原则、`FrameSlot` 线程解耦、SHA-256/128-bit 内容寻址 `.nsbc` 字节码缓存、宿主调用协议与按键路由管线。
2. **掌握 Nessa 语言与代数效应**：阅读 `[nessa-for-lava#top]`，快速掌握 Nessa 类型系统、`!Err T` 受检错误、`effect` / `handles(...)` 能力处理器以及驱动非阻塞 Minibuffer 的 `catch k` 定界延续。
3. **亲手编写第一个 Extension**：跟随 `[extension-development#top]`，从 `package.toml` 清单、`register_<name>_extension` 入口、服务发布、自定义交互式提示符到 `@lava_accel` 高速算子调用，完整走通插件研发与自动化测试闭环。

---

## Lava 官方手册全集（11 册） *lava-manuals-directory*

### 基础入门与系统架构 *dir-fundamentals*

- `[user-guide#top]` — **Lava 日常使用完全指南**（`*user-guide*`）
  涵盖 CLI 全部子命令与开关、5 分钟交互教程、缓冲区/分屏/标签页操作、**全部 29 项 `EditorOptions`（`:set` 语法与默认值详表）**、8 套内置主题调色板，以及 `~/.config/lava/init.ns` 实时定制范例。
- `[architecture#top]` — **Lava 架构设计与单虚拟机原理**（`*lava-architecture*`）
  详解 8 条核心架构原则、6 个 Rust 宿主 Crate（`lava-proto` / `lava-gtk` / `lava-vm` / `lava-accel` / `lava-term` / `lava-cli`）、`FrameSlot` 无锁帧交换、`.nsbc` 字节码缓存合成机制、代数效应与定界延续交互引擎，以及 5 级按键路由优先级。

### 核心交互与高频工作流 *dir-workflows*

- `[vim-and-emacs-keybindings#top]` — **Evil-Vim 与 Emacs 混合键位权威参考**（`*keybindings*`）
  完整收录 Evil 六大模式、光标移动、操作符、嵌套文本对象（`iw`/`aw`/`i"`/`i(`/段落等）、寄存器银行、局部与全局标记、键盘宏、分叉撤销树、全量 `:Ex` 命令表、Emacs `M-x`/`C-u`/`C-x` 组合键与 `<Space>` Leader 键位树。
- `[minibuffer-and-completion#top]` — **Minibuffer 深度指南：Vertico、Orderless、Marginalia 与 Consult**（`*minibuffer*`）
  解析纵向候选视窗（Vertico）、多分词乱序/前缀/后缀/`!取反` 评分器（Orderless）、右侧元数据徽章与快捷键反查（Marginalia）、实时缓冲区/配色/搜索预览与 `<C-g>` 无损回滚（Consult），以及两阶段上下文感知 `:Ex` 补全与分类历史记录。
- `[dirvish-and-ranger#top]` — **Dirvish 三栏文件管理器与 Ranger 操作手册**（`*dirvish*`）
  详解父目录/当前目录/实时语法高亮预览三栏 Miller 布局、进入方式（`-`、`:Dirvish`、`<Space>fd`）、完整 Ranger 键位（`h`/`j`/`k`/`l`、`gh`/`gr`/`gp`、`<Space>`/`v`/`uv` 多选标记、`yy`/`dd`/`pp`/`po`/`dD` 文件剪贴环、`cw`/`a`/`I`/`A` 智能重命名、`zh`/`zp`/`zf`/`on`/`os`/`oe`/`or` 过滤排序，以及 `S` 在当前目录打开终端）。
- `[terminal-and-pty#top]` — **原生 Linux POSIX PTY 与 VT100/TrueColor 终端手册**（`*terminal*`）
  剖析 `crates/lava-term` 伪终端与 VT100/ANSI 16/256/TrueColor 屏幕解析器、备用屏幕支持（直接运行 `htop`/`vim`/`less`/`git log`）、`1..9` 多实例与 4 种窗口布局（悬浮/底部分屏/右侧分屏/全窗口）、`TERM-INSERT` 直通模式与 `<C-\><C-n>` `TERM-NORMAL` 滚回浏览模式，以及 `:TermSend` / `<Space>ts` 代码行/选区投递。
- `[emacs-help-and-info#top]` — **Emacs 全局文档检索、`*Info*` 浏览器与自省系统指南**（`*emacs-help*`）
  全面介绍 Lava 自描述内省体系：`C-h i`（`*Info*` 超文本阅读器全套导航键）、`C-h d` / `:AproposDocs`（跨 Lava/Nessa 手册、命令、选项、服务与 `.ns` 符号的五域全局全文搜索）、`C-h a` / `C-h o`、`C-h f`（带 `[Source: file.ns:line]` 按 `s` 直跳源码）、`C-h v`（按 `e` 在线编辑选项）、`C-h m`、`C-h k` / `C-h c`、`C-h w`、`C-h p`、`K` 光标处智能帮助，以及 `:Telescope help_tags`（`<Space>fh`）与 `:Telescope docs`（`<Space>sH`）。

### 扩展生态与二次开发 *dir-ecosystem*

- `[plugins-catalog#top]` — **内置插件与核心包全景图鉴（40 Extensions + 4 Core Packages）**（`*plugins-catalog*`）
  按十大领域系统梳理全部 4 个核心包（`lava_sys`、`lava_core`、`lava_accel`、`lava_term`）与 **40 个内置 Extension**，逐一列出扩展 ID、发布服务键、核心命令、默认快捷键与职责边界。
- `[extension-development#top]` — **Lava 扩展插件开发指南**（`*extension-dev*`）
  手把手演示如何用 Nessa 编写一等公民 Extension：`package.toml` 规范、`register_<name>_extension` 自动发现机制、服务注册与反向依赖保护、钩子监听、基于 `ask_prompt` 定界延续的多步交互式命令、侧边栏/浮窗/行内装饰 API、`@lava_accel` 原生加速接口以及确定性单元/无头测试方法。
- `[nessa-for-lava#top]` — **面向 Lava 配置与插件开发的 Nessa 语言手册**（`*nessa-for-lava*`）
  聚焦在 `init.ns` 与 Extension 开发中最核心的 Nessa 语法特性：基本类型与集合、结构体/枚举/`newtype`/模式匹配、`?T` 可选值与 `!Err T` 错误传播、`effect` 代数效应与 `handles(...)` 能力结构体、`catch k` 多提示符定界延续、`async effect` 与 `Future` 并发分片，以及 `std` 标准库速查。

---

## Nessa 语言底层规范手册索引（跨仓库联动） *nessa-language-manuals*

Lava 的文档索引器（`lava-accel::index_docs`）同时索引了 Nessa 编译器仓库（`nessa:*` 命名空间）的设计与语言规范文档，您可以在 `*Info*` 或 `:AproposDocs` 中直接跳转查阅：

- `[nessa:grammar/intro#top]` — **Nessa 语法总览**（词法 `[nessa:grammar/lexical#top]`、字面量 `[nessa:grammar/literals#top]`、表达式 `[nessa:grammar/expressions#top]`、语句 `[nessa:grammar/statements#top]`、模式 `[nessa:grammar/patterns#top]`、定义 `[nessa:grammar/definitions#top]`）
- `[nessa:type-system/intro#top]` — **Nessa 类型系统导论**（内建类型 `[nessa:type-system/builtin-types#top]`、结构体 `[nessa:type-system/data-types/struct#top]`、枚举 `[nessa:type-system/data-types/enum#top]`、`newtype` 与类型别名 `[nessa:type-system/newtype-and-typealias#top]`、Trait 与扩展 `[nessa:type-system/trait-implementation-extension/intro#top]`）
- `[nessa:algebraic-effect-and-multi-prompt-delimited-continuation/intro#top]` — **代数效应与多提示分界续延总览**（效应语法 `[nessa:algebraic-effect-and-multi-prompt-delimited-continuation/algebraic-effect#top]`、Capability 能力模式 `[nessa:algebraic-effect-and-multi-prompt-delimited-continuation/capability#top]`、定界延续 `[nessa:algebraic-effect-and-multi-prompt-delimited-continuation/multi-prompt-delimited-continuation#top]`）
- `[nessa:concurrency-and-async-algebraic-effect/intro#top]` — **结构化并发与异步代数效应**
- `[nessa:code-orgnization/package-management#top]` — **Nessa 包管理、模块发现与 `@pkg` 跨包导入规范**
- `[nessa:standard-library/overview#top]` — **Nessa 标准库（`std`）22 个核心模块全景参考**
- `[nessa:dev/architecture#top]` — **Nessa 编译器、NIR、NSBC 字节码与运行时引擎架构**

---

## 快速查找标签索引提示 *help-tag-tips*

在 Lava 中，除了浏览上述章节目录，您还可以使用以下任一方式按 `*help-tag*` 或任意关键词秒级直达文档段落：

1. 执行 `:h <tag>`（例如 `:h orderless`、`:h ranger-keys`、`:h nsbc-cache`、`:h term-modes`）。
2. 按 `<Space> f h`（`:Telescope help_tags`）模糊搜索所有手册标题与 `*tag*` 锚点并实时预览。
3. 按 `C-h d`（`:AproposDocs`）或 `<Space> s H`（`:Telescope docs`）输入任意中文或英文关键词（如 `UndoTree`、`continuation`、`Miller`、`TrueColor`），一次性横跨手册正文、命令、配置项、服务与 `.ns` 源码定位答案。
