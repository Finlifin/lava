# Lava 日常使用完全指南 *user-guide*

本手册面向日常开发场景，系统介绍 `lava` 命令行工具、启动后前 5 分钟入门教程、缓冲区/分屏/标签页管理、**全部 29 项 `EditorOptions` 配置与 `:set` 语法详表**、8 套内置主题调色板切换，以及 `~/.config/lava/init.ns` 个人脚本定制。返回手册总目录请参阅 `[index#top]`。

---

## 命令行界面（CLI）完整参考 *cli-options*

`lava` 二进制支持 4 种运行子命令与丰富的窗口/调试选项：

### 子命令一览 *cli-subcommands*

| 命令形式 | 说明 |
| --- | --- |
| `lava [OPTIONS] [FILES...]` | 默认启动模式：加载单 Nessa VM、打开指定文件（无文件参数时显示 `*Dashboard*` 欢迎页或 `*scratch*` 缓冲区）并启动 GTK4 窗口（或无头会话）。支持 `--` 分隔以 `-` 开头的文件名。 |
| `lava check [OPTIONS]` | 在单 VM 中完整组合并静态类型检查所有已发现的核心包、内置扩展、`--ext-dir` 用户包及 `init.ns`，同时预热 `.nsbc` 字节码缓存并报告包数量。 |
| `lava list-extensions [OPTIONS]` | （别名 `lava extensions`）按拓扑打印工作区内已发现的所有包，标注来源（`[core]` / `[builtin]` / `[user]`）、版本号、依赖与简介。 |
| `lava run-package <PKG_DIR> [OPTIONS]` | 在配备 `LavaHostHandler` 的 Nessa VM 中直接运行指定的独立 `exe` 或 `lib` 包目录。 |

### 命令行标志与环境变量 *cli-flags*

| 选项标志 | 默认值 | 功能说明 |
| --- | --- | --- |
| `-u, --init <PATH>` | 自动查找 | 显式加载指定的用户 Nessa 初始化脚本（例如 `lava -u .lava/init.ns`）；传入 `-u NONE` 等同于 `-q` 禁用初始化脚本。 |
| `-q, --no-init` | `false` | 跳过默认的 `$XDG_CONFIG_HOME/lava/init.ns` 与 `~/.config/lava/init.ns`，以纯净配置启动。 |
| `--ext-dir <DIR>` | 空（可重复） | 挂载额外的用户扩展目录（既可指向单个含 `package.toml` 的包目录，也可指向包含多个子包的父目录）。同名限定包会覆盖内置插件。 |
| `--workspace <DIR>` | 自动探测 | 显式指定 Lava 引擎工作区根目录（也可通过环境变量 `LAVA_WORKSPACE` 指定）。 |
| `--entry <PKG>` | `lava.org/ext_app` | 覆盖顶层会话入口包名称。 |
| `--headless` | `false` | 不创建 GTK4 窗口，在无图形服务器的终端或 CI 环境中运行完整编辑器逻辑。 |
| `--keys <KEYS>` / `--headless-keys <KEYS>` | `""` | 启动后自动按序注入按键流（支持 `<Escape>`、`<Return>`、`<Space>`、`<C-x>`、`<M-x>` 等记法）。 |
| `--dump-frame-png <PNG>` | 无 | 会话退出时调用 PangoCairo 将最后一帧 `RenderFrame` 精确光栅化保存为 PNG 图像。 |
| `--width <PX>` | `1280` | 初始窗口像素宽度（最小 `64`）。 |
| `--height <PX>` | `800` | 初始窗口像素高度（最小 `64`）。 |
| `-h, --help` / `-V, --version` | — | 打印命令行帮助或版本信息。 |

---

## 前 5 分钟快速上手教程 *quickstart*

首次运行 `./target/release/lava` 时，若未在命令行指定文件名，Lava 默认呈现交互式 **`*Dashboard*`** 启动面板（由 `ext_persistence` 提供，按 `f` 找文件、`g` 全局搜索、`n` 新建空白缓冲区、`s` 恢复上次会话、`l` 打开扩展管理器、`q` 关闭面板）。

跟着下面 6 步即可走通核心日常工作流：

1. **打开文件**：按 `<Space> f f`（Minibuffer Orderless 路径补全）、`<Space> s f`（Telescope 双浮窗带代码预览）或 Emacs 组合键 `C-x C-f`，输入几个不连续的字母（如 `doc lib`）后按 `<Return>` 打开文件。
2. **模态与无模态编辑**：Lava 默认处于 Evil **Normal** 模式（块状光标 `Block`）。按 `i` 进入 **Insert** 模式（竖线光标 `Beam`）输入代码，按 `<Escape>` 返回 Normal 模式；若偏爱纯 Emacs 操作，随时按 `<C-z>` 即可切换至无模态 Emacs 模式（再次按 `<C-z>` 切回 Evil 模式，详见 `[vim-and-emacs-keybindings#top]`）。
3. **保存与撤销**：按 `<Space> f s`、`C-x C-s` 或输入 `:w<Return>` 保存当前文件。按 `u` 撤销、`<C-r>` 重做；若撤销后产生新编辑分支，按 `g-` / `g+` 在不同历史分支间自由穿梭，或按 `<Space> u` 打开 `*UndoTree*` 图形化分叉树！
4. **浏览目录树**：在任意代码缓冲区直接按 `-`，立即在当前文件所在目录打开 **Dirvish** 三栏 Miller Columns 文件管理器（`h`/`j`/`k`/`l` 极速穿梭，详见 `[dirvish-and-ranger#top]`）。
5. **呼出内置终端**：按 `<C-\>` 弹出居中悬浮 POSIX PTY 真实终端运行 `cargo test` 或 `git status`，再次按 `<C-\>` 隐藏终端且后台进程保持存活（详见 `[terminal-and-pty#top]`）。
6. **随时查询帮助与退出**：遇到不熟悉的按键按 `C-h k` 再按目标键即可查看其绑定的命令与源码位置；按 `C-h i` 阅读本手册集（详见 `[emacs-help-and-info#top]`）；输入 `:wq<Return>`（保存退出）、`:q!<Return>`（放弃修改强制退出）或按 `C-x C-c` 退出 Lava。

---

## 缓冲区、分屏窗口与标签页管理 *buffers-windows-tabs*

Lava 的工作空间由三层正交结构组成：**全局共享的缓冲区池（`Buffer`）**、**每个标签页独立的二叉树分屏窗格（`WindowPane`）** 以及 **多标签页工作区（`TabWorkspace`）**。

### 缓冲区（Buffer）管理 *buffer-management*

每个打开的文件或特殊面板（如 `*scratch*`、`*Messages*`、`*Info*`、`*magit*`、`*terminal-1*`）都是 `EditorWorld` 中的一个 `Buffer`：

| 操作目标 | Evil / Ex 方式 | Leader / Emacs 方式 |
| --- | --- | --- |
| 打开或新建文件 | `:e <path>` / `:edit <path>` | `<Space> f f` / `C-x C-f` |
| 切换缓冲区（含实时预览） | `:b <name_or_id>` | `<Space> b b` / `<Space> s b` / `C-x b` |
| 下一个 / 上一个缓冲区 | `:bn` (`:bnext`) / `:bp` (`:bprev`) | — |
| 查看缓冲区列表面板 | `:ls` / `:buffers` | `C-x C-b`（在 `*Buffers*` 中按 `<Return>` 跳转） |
| 关闭当前缓冲区 | `:bd`（未保存保护） / `:bd!`（强制） | `C-x k` (`kill-buffer`) |
| 窄化（Narrow）仅聚焦选区 | `:'<,'>` 配合命令或 `<Space> z n` | `C-x n n` 窄化到选区 / `C-x n w` (`widen`) 恢复全文 |

### 二叉树分屏窗口（Window Splits）管理 *window-management*

`ext_window` 支持任意深度的水平与垂直二分窗格树，支持键盘几何焦点跳转与鼠标点击聚焦、滚轮滚动（每次 3 行）、双击选词、三击选行与拖拽选区：

| 操作目标 | `<C-w>` 窗口前缀 | Leader / Ex / Emacs 方式 |
| --- | --- | --- |
| 上下水平分屏 | `<C-w> s` | `<Space> w s` / `:sp [file]` / `C-x 2` |
| 左右垂直分屏 | `<C-w> v` | `<Space> w v` / `:vsp [file]` / `C-x 3` |
| 聚焦左 / 下 / 上 / 右侧窗格 | `<C-w> h` / `j` / `k` / `l` | — |
| 循环切换下一个窗格 | `<C-w> w` | `C-x o` (`other-window`) |
| 关闭当前分屏窗格 | `<C-w> c` / `<C-w> q` | `<Space> w d` / `:close` / `C-x 0` |
| 仅保留当前窗格（关闭其余分屏） | `<C-w> o` | `:only` / `C-x 1` |
| 等比例均分所有窗格尺寸 | `<C-w> =` | `M-x balance-windows` |
| 增高 / 压低当前窗格比例 | — | `C-x ^` (`enlarge-window`) / `M-x shrink-window` |

### 多标签页工作区（Tab Workspaces）管理 *tab-management*

每个标签页拥有自己独立的分屏布局树与焦点窗格，当打开超过 1 个标签页时顶部自动渲染标签栏（支持鼠标点击切换）：

- **新建标签页**：`:tabnew [file]`（或在 Dirvish 中按 `gn` 在新标签页打开目录）
- **切换下一个 / 上一个标签页**：`:tabn` (`:tabnext`) / `:tabp` (`:tabprev`)
- **关闭当前标签页**：`:tabc` (`:tabclose`)

---

## 全部 29 项编辑器选项参考（`EditorOptions` 与 `:set`） *options*

`ext_document::EditorOptions` 共定义了 **29 个强类型全局配置字段**。您既可以在运行期通过 `:set < token ... >`（支持在单条 `:set` 命令中空格分隔串联多项，如 `:set rnu ts=2 sw=2 wrap`）或 `C-h v`（按 `e` 在线切换/编辑）即时修改，也可以在 `init.ns` 中直接读写 `world.options.<field>`：

| # | `EditorOptions` 字段名 | 类型 | 默认值 | `:set` 命令语法（开启 / 关闭 / 赋值） | 详细语义说明 |
| --- | --- | --- | --- | --- | --- |
| 1 | `number` | `bool` | `true` | `:set nu` (`number`) / `nonu` (`nonumber`) / `nu!` | 在窗口左侧槽（Gutter）显示行号 |
| 2 | `relativenumber` | `bool` | `false` | `:set rnu` (`relativenumber`) / `nornu` / `rnu!` | 启用混合相对行号（默认关闭，仅显示绝对行号；开启后其余行显示距离光标的行距，便于 `5j`/`3dk`） |
| 3 | `tabstop` | `i64` | `4` | `:set ts=<N>` / `:set tabstop=<N>`（`N >= 1`） | 制表符 `\t` 的显示列宽及 `<Tab>` 插入宽度 |
| 4 | `shiftwidth` | `i64` | `4` | `:set sw=<N>` / `:set shiftwidth=<N>`（`N >= 1`） | `>>`、`<<` 操作符、自动缩进与智能退格每次移动的空格列数 |
| 5 | `expandtab` | `bool` | `true` | `:set et` (`expandtab`) / `:set noet` (`noexpandtab`) | 按 `<Tab>` 与自动缩进时使用空格替代 `\t` 字符 |
| 6 | `ignorecase` | `bool` | `true` | `:set ic` (`ignorecase`) / `:set noic` (`noignorecase`) | 搜索（`/`、`?`、`C-s`、`:s`、Live Grep）默认忽略大小写 |
| 7 | `smartcase` | `bool` | `true` | `:set scs` (`smartcase`) / `:set noscs` (`nosmartcase`) | 当 `ignorecase=true` 且搜索词包含大写字母时，自动升级为大小写敏感匹配 |
| 8 | `wrap` | `bool` | `false` | `:set wrap` / `:set nowrap` | 超出窗格宽度时软折行显示（`false` 时水平平移视口） |
| 9 | `hlsearch` | `bool` | `true` | `:set hls` (`hlsearch`) / `:set nohls`（临时清除用 `:noh`） | 高亮渲染当前缓冲区内所有匹配最近一次搜索模式的片段 |
| 10 | `incsearch` | `bool` | `true` | `:set is` (`incsearch`) / `:set nois` (`noincsearch`) | 在 `/`、`?` 与 `C-s` 输入过程中实时跳转预览首个匹配并高亮全文命中 |
| 11 | `scrolloff` | `i64` | `5` | `:set so=<N>` / `:set scrolloff=<N>`（`N >= 0`） | 光标上下方始终保留的最小上下文可视行数 |
| 12 | `sidescrolloff` | `i64` | `8` | `:set siso=<N>` / `:set sidescrolloff=<N>`（`N >= 0`） | `nowrap` 模式下光标左右侧保留的最小水平上下文列数 |
| 13 | `cursorline` | `bool` | `true` | `:set cul` (`cursorline`) / `:set nocul` (`nocursorline`) | 使用主题 `bg_cursor_line` 底色高亮当前光标所在行，并高亮当前行号 |
| 14 | `signcolumn` | `bool` | `true` | `:set signcolumn` / `:set nosigncolumn` | 在行号左侧预留固定符号列，用于显示 Git 增删改色条、Trouble 诊断与 TODO 徽章 |
| 15 | `indent_guides` | `bool` | `true` | `:set indentguides` / `:set noindentguides` | 按 `shiftwidth` 步长绘制跨越代码块内空行的连续垂直缩进对齐细线，并用主题强调色高亮当前作用域层级 |
| 16 | `match_brackets` | `bool` | `true` | `init.ns` / `C-h v` 修改 `match_brackets` | 当光标停留在 `()`、`[]`、`{}` 上时自动高亮配对括号 |
| 17 | `auto_indent` | `bool` | `true` | `:set ai` (`autoindent`) / `:set noai` (`noautoindent`) | 按 `<Return>` 或 `o` 新建行时继承上一行缩进，并在 `{`/`(`/`:` 后智能增加一级缩进 |
| 18 | `autopairs` | `bool` | `true` | `:set autopairs` / `:set noautopairs`（或 `M-x autopairs-toggle`） | Insert 模式下自动补全成对括号/引号、右括号跳过、空格对称衬垫与成对退格删除 |
| 19 | `clipboard_unnamedplus` | `bool` | `true` | `:set clipboard=unnamedplus` / `:set clipboard=` | 将默认无名寄存器 `""` 与系统剪贴板（`+` / `*`）双向实时同步 |
| 20 | `mouse_enabled` | `bool` | `true` | `init.ns` / `C-h v` 修改 `mouse_enabled` | 启用鼠标单击定位、双击选词、三击选行、滚轮滚动及拖拽进入 Visual 选区 |
| 21 | `zen_mode` | `bool` | `false` | `:set zen` / `:set nozen`（或 `<Space> z` / `:ZenMode`） | 开启无干扰居中禅模式，隐藏多余边距装饰并将编辑区居中约束在 `zen_width` 列宽 |
| 22 | `zen_width` | `i64` | `92` | `init.ns` / `C-h v` 修改 `zen_width` | `zen_mode` 开启时居中编辑区的目标字符列宽 |
| 23 | `twilight_mode` | `bool` | `false` | `:set twilight` / `:set notwilight`（或 `<Space> u t` / `:Twilight`） | 聚光灯模式：自动调暗当前代码块/段落以外的上下文文本，强化视觉专注 |
| 24 | `format_on_save` | `bool` | `false` | `:set formatonsave` / `:set noformatonsave`（或 `<Space> u f`） | 执行保存（`:w` / `save-buffer`）前自动调用 `ext_conform` 格式化当前缓冲区 |
| 25 | `theme_name` | `String` | `"catppuccin-mocha"` | `:colorscheme <name>` / `:colo <name>` / `:theme <name>` | 当前激活的配色方案名称（共 8 套内置调色板，见下一节） |
| 26 | `font_family` | `String` | `"Monospace"` | `init.ns` / `C-h v` 修改 `font_family` | PangoCairo 代码渲染使用的等宽字体族名称（如 `"JetBrains Mono"`、`"Fira Code"`） |
| 27 | `font_size_pt` | `f64` | `13.0` | `init.ns` / `C-h v` 修改 `font_size_pt` | 代码与界面等宽文本字号（单位 pt），修改后由 `host_measure_font` 自动重算网格度量 |
| 28 | `cursor_blink` | `bool` | `true` | `init.ns` / `C-h v` 修改 `cursor_blink` | 控制活动窗格光标闪烁指示标志 |
| 29 | `evil_mode` | `bool` | `true` | 按 `<C-z>` / `M-x toggle-evil-mode` / `C-h v` | 为 `true` 时启用 Evil-Vim 模态编辑；为 `false` 时切换为纯 Emacs 无模态编辑 |

> **小贴士**：直接输入不带参数的 `:set<Return>` 会在状态栏回显常用核心开关摘要；若想以交互方式搜索所有 29 个选项的当前值、默认值并按 `e` 直接切换/输入新值，请按 `C-h v`（`describe-variable`）！

---

## 内置配色方案与实时预览（Colorschemes） *themes*

`ext_render` 内置 **8 套精心调校的 24-bit 真彩色主题调色板**（涵盖背景层级、行号槽、模式徽章、7 类语法高亮语义色与 3 级诊断色）：

| 主题名称（精确标识符） | 风格特征 |
| --- | --- |
| `catppuccin-mocha` | **默认主题** — 柔和暖调深色粉彩，长时间编码极度舒适 |
| `tokyo-night` | 现代东京都市深蓝紫霓虹高对比度暗色主题 |
| `rose-pine` | 低饱和松木暗紫与暖金点缀的静谧暗色主题 |
| `kanagawa` | 灵感源自葛饰北斋浮世绘的墨黑与和纸暖米色暗调主题 |
| `nord` | 经典极地冰蓝冷色调护眼暗色主题 |
| `gruvbox-dark` | 复古暖棕高辨识度经典暗色主题 |
| `one-dark` | 清爽均衡的现代工业暗灰配色 |
| `solarized-light` | 专为明亮室内光线设计的经典暖米底色高可读浅色主题 |

### 三种切换与实时预览主题的方式 *switching-themes*

1. **Minibuffer Consult 实时预览**：输入 `:colorscheme `（或 `:colo ` / `:theme `），按 `<C-n>` / `<C-p>` 或 `<Up>` / `<Down>` 上下移动候选条目时，**整个编辑器窗口会实时渲染对应主题**；按 `<Return>` 确认应用，按 `<Escape>` 或 `<C-g>` 立即无损回滚至原主题！
2. **Telescope 配色选择器**：按 `<Space> s c`（`:Telescope colorscheme`），在双浮窗中边看 Nessa 代码示例边实时切换整窗主题。
3. **循环切换**：执行 `M-x cycle-theme` 在核心主题序列间一键轮换。

---

## 使用 `init.ns` 定制个人环境与实时扩展 *init-ns*

Lava 启动时默认自动加载 `$XDG_CONFIG_HOME/lava/init.ns` 或 `~/.config/lava/init.ns`；对于特定项目，您也可以将项目专属配置放在 `.lava/init.ns` 并通过 `lava -u .lava/init.ns` 加载。

与传统编辑器使用静态 JSON/TOML 配置不同，`init.ns` 是一段**运行在同一个单 Nessa VM 中的一等公民 Nessa 模块**（合成为 `user.local/user_init`）。只需导出顶层函数 `pub fn configure(kernel: SystemKernel) -> !String Unit`，即可一次性完成选项调整、主题设定、自定义交互命令注册与快捷键绑定：

```nessa
-- ~/.config/lava/init.ns
use @lava_core.{
    ExtId, InteractiveEffects, PromptRequest, SystemKernel,
    ask_prompt, log_event, status_emit
}
use @ext_document.{BufferKind, Pos, require_editor_world}
use @ext_render.{SERVICE_RENDERER, RenderCompositor}

pub fn user_custom_id() -> ExtId = ExtId("user.local/my_config")

pub fn configure(kernel: SystemKernel) -> !String Unit {
    let world = require_editor_world(kernel)!

    -- 1. 直接以强类型字段配置 EditorOptions
    world.options.font_family = "JetBrains Mono"
    world.options.font_size_pt = 14.0
    world.options.relativenumber = false
    world.options.tabstop = 4
    world.options.shiftwidth = 4
    world.options.scrolloff = 8
    world.options.format_on_save = true

    -- 2. 切换默认启动配色方案
    let rc_opt = kernel.service(SERVICE_RENDERER)
    if rc_opt != null {
        let _ = rc_opt.as(RenderCompositor).set_theme("tokyo-night", world)!
    }

    -- 3. 注册个人专属 Extension、基于定界延续的命令与 Leader 快捷键
    let ext_id = user_custom_id()
    kernel.register_extension(
        ext_id,
        "user.local",
        "my_config",
        "0.1.0",
        "Personal Lava customizations and quick scratch workflow.",
        [ExtId("lava.org/ext_document")],
        |k: SystemKernel| -> !String Unit {
            k.register_command(
                "my-daily-journal",
                ext_id,
                "Prompt for a topic and open an Org journal buffer.",
                |kern: SystemKernel, _args: List| -> #InteractiveEffects !String Any {
                    let w = require_editor_world(kern)!
                    let topic? = ask_prompt(PromptRequest.text("Journal topic: ", ""))# else {
                        return ()
                    }
                    let buf = w.create_buffer(
                        "*journal:" ++ topic ++ "*",
                        BufferKind.scratch,
                        major_mode = "org",
                        initial_lines = ["* TODO " ++ topic, ""],
                    )
                    buf.point = Pos.new(1, 0)
                    let _ = w.switch_to_buffer(buf.id)
                    kern.emit_hook("buffer-switched", buf.id)
                    status_emit("Opened journal: " ++ buf.name)#
                    ()
                },
            )

            let leader = k.ensure_keymap("vim-leader")
            leader.bind("<Space> n j", "my-daily-journal", ext_id, description = "New Org journal")
            ()
        },
        |_k: SystemKernel| -> !String Unit { () },
    )!
    kernel.activate_extension(ext_id)!
    ()
}
```

编写或修改 `~/.config/lava/init.ns` 后，您随时可以运行 `lava check` 在毫秒级验证类型与依赖是否正确并预热 `.nsbc` 缓存。仓库自带的完整可运行示例详见 `examples/init.ns`，更多插件开发细节请继续阅读 `[extension-development#top]` 与 `[nessa-for-lava#top]`。

---

## 五、SSH 远程开发（服务端 `clangd` / `lava-server`）与管道式 Diff 速览 *remote-and-diff-quickstart*

### 1. SSH Remote 与服务端 LSP（`clangd` / `rust-analyzer`）
- **直接打开与保存远端文件**：在命令行或 `:e` / `C-x C-f` 中直接输入 `ssh://user@host/path/to/main.cpp`（或 Emacs TRAMP 语法 `/ssh:user@host:/path/to/main.cpp`），Minibuffer 会自动补全 `~/.ssh/config` 主机名与远端目录路径；按 `:w` 或 `C-x C-s` 直接写回远端。
- **独立无头服务端 `lava-server`**：远端若安装了 `lava-server`（位于 `~/.local/bin/lava-server` 或 `$PATH`，编译自独立 Nessa 集合 `server/`），Lava 会自动通过持久 `lava-server --stdio` JSON-RPC 2.0 通道加速文件、Git 与搜索操作；若远端未安装，则自动无感回退到免安装的 POSIX SSH ControlMaster 模式（也可执行 `:RemoteDeploy` 探测状态）。
- **服务端运行 `clangd` 等 LSP**：打开远端 C/C++、Rust、Python、Go 等源码时，`ext_lspconfig` 与 `ext_lsp` 会自动在**远端服务器**探测项目根目录（如 `compile_commands.json`、`Cargo.toml`）并在服务端直接拉起 `clangd` / `rust-analyzer`，自动完成 `ssh://host/path` <-> `file:///path` 双向 URI 映射，支持跨远端系统头文件的 `gd` 跳转、悬停文档与实时诊断。
- **常用快捷键**：`<Space> S c`（`:RemoteConnect` 连接主机）、`<Space> S b`（`:RemoteBrowse` 远程 Ranger 键位目录树）、`<Space> S f`（`:RemoteFiles` 远端找文件）、`<Space> S g`（`:RemoteGrep` 远端 `rg` 搜索）、`<Space> S s`（`:RemoteShell` 远端真 PTY 终端）、`<Space> S p`（`:RemotePortForward` 端口转发）、`<Space> S i`（`:RemoteStatus` 面板）。

### 2. 带 stdio `Mapper` 与 `Filter` 的高级 Diff（`sed` / `awk` / `rg`）
- **启动对比**：`:DiffFiles <left> <right>`（`<Space> d f`）、`:DiffBuffers`（`<Space> d b`）、`:DiffGit`（`<Space> d g`）、`:DiffSaved`（`<Space> d s`）或 `:DiffDir <dirA> <dirB>`（`<Space> d d`）。
- **`Filter`（通过 stdio 过滤行，保留原始行号）**：执行 `:DiffFilter rg -v '^DEBUG:'`（或在 Diff 窗口按 `f`）即可用 `rg`、`grep`、`awk` 剔除噪声行或只保留关心的行，Diff 行号栏仍精确显示源文件中的真实行号。
- **`Mapper`（通过 stdio 1:1 变换行文本，支持原文本切换）**：执行 `:DiffMap sed -E 's/[0-9]{4}-[0-9]{2}-[0-9]{2}/<DATE>/g'`（或在 Diff 窗口按 `m`）即可用 `sed` / `awk` 抹平时间戳、PID、内存地址后再对齐比较；按 `v` 键可随时在归一化文本（`[VIEW: MAPPED]`）与原始未改动文本（`[VIEW: ORIGINAL]`）之间一键切换，按 `P` 可快速选用内置 `sed`/`awk`/`rg` 预设，按 `t` 切换双栏同步分屏与 Unified 视图，按 `do` / `dp` 在两侧缓冲区之间互推 Hunk。
