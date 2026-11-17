# Lava 扩展插件开发实战指南 *extension-dev*

在 Lava 中，每一个内置功能模块（从 `ext_document`、`ext_vim` 到 `ext_telescope`、`ext_dirvish`）与您自己编写的插件共享 **100% 相同的单 Nessa VM 运行时与内核 API**。没有沙箱阉割，没有缓慢的 JSON-RPC 跨进程桥接——您编写的任何 Extension 都是一等公民 Nessa 库包，能够直接读写 `EditorWorld`、发布强类型服务、利用定界延续（Delimited Continuations）编写无回调的交互式提示流、渲染侧边栏/浮窗/行内装饰，并直接调用 `@lava_accel` 的 Rust 多核加速算子。返回手册总目录请参阅 `[index#top]`；若需温习 Nessa 语法细节请先参阅 `[nessa-for-lava#top]`。

---

## 插件目录结构与自动发现机制 *extension-lifecycle*

一个标准的独立 Lava Extension 是一个 `type = "lib"` 的 Nessa 包，既可以放在工作区内置目录 `extensions/ext_<name>/`，也可以放在任意外部目录并通过 `lava --ext-dir /path/to/my_exts` 加载（若只是轻量个人定制，甚至无需建包，直接在 `~/.config/lava/init.ns` 的 `pub fn configure(kernel: SystemKernel)` 中调用 `kernel.register_extension(...)` 即可，详见 `[user-guide#top]` 与 `[Info: *init-ns*]`）。

### `package.toml` 清单规范 *extension-manifest*

新建目录 `ext_wordcount/` 并创建 `package.toml`：

```toml
[package]
name = "ext_wordcount"
domain = "user.local"
version = "0.1.0"
type = "lib"

[dependencies]
"lava.org/lava_core" = "0.1.0"
"lava.org/ext_document" = "0.1.0"
"lava.org/ext_vim" = "0.1.0"
```

### 自动引导发现约定（`register_<stem>_extension`） *auto-discovery-rule*

当 `lava-vm` 组合器（`crates/lava-vm/src/composer.rs`）构建单 VM 根包 `lava.org/lava_boot` 时，会自动扫描所有 `extensions/*` 与 `--ext-dir` 包入口文件（`src/lib.ns`）中位于第 0 列的顶层公开函数。只要满足以下签名之一，组合器就会将其自动编入启动序列并在其后调用 `kernel.activate_all()!`：

- `pub fn register_<stem>_extension(kernel: SystemKernel) -> !String Unit`（推荐命名，例如包名 `ext_wordcount` 对应 `register_wordcount_extension`）；
- `pub fn register_<pkg_name>(kernel: SystemKernel) -> !String Unit`；
- `pub fn register_extension(kernel: SystemKernel) -> !String Unit`（返回类型也可不带 `!String` 错误集）。

### `ExtensionDescriptor` 拓扑激活与干净卸载契约 *lifecycle-contract*

调用 `kernel.register_extension(id, domain, name, version, description, deps, activate_fn, deactivate_fn)` 注册扩展后，`SystemKernel` 提供严格的生命周期保障：

1. **DFS 环检测与拓扑排序**：`activate_extension` 先递归激活 `deps: List`（元素为 `ExtId`）中声明的所有前置扩展；若检测到循环依赖则立即返回可读错误诊断。
2. **反向依赖守卫**：在 `*Lazy*` 管理器（`<Space>l`）或代码里调用 `kernel.deactivate_extension(ext_id)` 时，若仍有其他处于 `ExtState.active` 状态的扩展依赖当前扩展，内核会拒绝停用以维持单 VM 不变量。
3. **资源按所有者自动回收**：一旦停用成功，内核无需插件手动逐条删除，**自动卸载该 `ExtId` 名下发布的所有 Services、Commands、Hooks 以及所有 Keymap 中的按键绑定**！插件的 `deactivate` 回调只需负责清理它在 `EditorWorld` 中挂载的 UI 装饰（例如调用 `world.clear_owner_ui("ext_wordcount")`）。

---

## 服务注册总线、命令与生命周期钩子 *services-and-hooks*

### 发布与查询强类型服务（Services） *kernel-services*

插件不应使用可变的模块级全局变量保存会话状态，而应定义自己的状态结构体并发布到 `SystemKernel` 的服务注册表中：

```nessa
use @lava_core.{ExtId, SystemKernel}

pub const SERVICE_WORDCOUNT: String = "user.wordcount.state"
pub fn ext_wordcount_id() -> ExtId = ExtId("user.local/ext_wordcount")

pub struct WordCountState {
    pub enabled: bool,
    pub last_words: i64,
}

pub fn require_wordcount_state(kernel: SystemKernel) -> !String WordCountState {
    let raw = kernel.require_service(SERVICE_WORDCOUNT)!
    raw.as(WordCountState)
}
```

常用内核核心服务键速查：
- `"lava.document.world"` -> `EditorWorld`（`require_editor_world(kernel)!`）
- `"lava.window.manager"` -> `WindowManager`（`require_window_manager(kernel)!`）
- `"lava.render.compositor"` -> `RenderCompositor`
- `"lava.vim.state"` -> `VimState`（`require_vim_state(kernel)!`）
- `"lava.emacs.state"` -> `EmacsState`（`require_emacs_state(kernel)!`）
- `"lava.minibuffer.controller"` -> `MinibufferController`

### 注册命令与多模式键位绑定 *commands-and-keymaps*

- **交互式命令**：调用 `k.register_command("word-count", ext_id, "Count words in active buffer.", handler_fn)`。默认 `interactive = true`，自动出现在 `M-x`、`:word-count`、`C-h f` 与 `:Apropos` 中。
- **专用 `:Ex` 大写别名**：若想同时支持 `:WordCount`，可额外注册 `"ex:WordCount"` 并传具名参数 `interactive = false`（避免在 `M-x` 中重复列出，同时被 `:Ex` 补全器一等识别！）。
- **常用键位表（Keymaps）**：
  - `"vim-leader"`（`KEYMAP_VIM_LEADER`）：绑定 `"<Space> c w"` 等 Leader 序列，自动接入实时 Which-Key 提示栏；
  - `"vim-normal"` / `"vim-visual"` / `"vim-insert"`：绑定各模态快捷键（如 `"g s"`、`"] d"`、`"<C-n>"`）；
  - `"emacs-global"`：绑定全局 `C-x ...` 或 `M-...` 组合键；
  - `"mode:<major_mode>"`（如 `"mode:wordcount-report"`）：**缓冲区局部主模式键位表**！只要当前缓冲区的 `buf.major_mode == "wordcount-report"`，Normal 模式下的单键（如 `q` 关闭、`r` 刷新、`<Return>` 跳转）自动优先路由至该表，完全不影响普通代码编辑缓冲区！

### 核心事件钩子（Hooks）一览 *kernel-hooks*

通过 `k.add_hook(hook_name, ext_id, |kern: SystemKernel, payload: Any| -> Unit { ... })` 订阅编辑器实时动态：

| 钩子名称 (`hook_name`) | `payload` 运行时类型 | 触发时机 |
| --- | --- | --- |
| `"extension-activated"` / `"extension-deactivated"` | `ExtId` | 任意扩展完成激活或停用 |
| `"buffer-switched"` | `BufferId` | 活动缓冲区切换或新文件打开 |
| `"buffer-modified"` | `BufferId` | 缓冲区文本内容发生增删变更 |
| `"before-save"` / `"buffer-saved"` | `BufferId` | 文件落盘保存前（如 `format_on_save`）与保存成功后（如刷新 Git 状态） |
| `"cursor-moved"` | `BufferId` | 活动缓冲区光标位置移动 |
| `"mode-changed"` | `String` | 编辑模式切换（`"NORMAL"` / `"INSERT"` / `"EMACS"` 等） |
| `"vim-yank"` | `RegisterValue` | 执行 `y` 复制操作后（驱动复制区域高亮与剪贴历史） |
| `"after-insert-stroke"` | `KeyStroke` | Insert 模式每次按键落入缓冲区后（驱动 `ext_cmp` 自动弹出补全） |
| `"theme-changed"` | `String` | 配色方案切换后 |
| `"sidebar-clicked"` | `(String, i64)` | 用户鼠标点击左/右侧边栏某行，传入 `(owner, row_idx)` |
| `"window-focused"` | `WinId` | 鼠标或键盘切换焦点分屏窗格 |

---

## 使用定界延续编写多步交互式提示（`ask_prompt`） *custom-prompts*

得益于 `lava_core::ask_prompt` 的 `catch k` 定界延续机制，您的命令可以直接用**同步顺序直写风格**连续发起多次非阻塞 Minibuffer 提问（自动享受 Vertico + Orderless + Marginalia）：

```nessa
use @lava_core.{InteractiveEffects, PromptKind, PromptRequest, SystemKernel, ask_prompt, status_emit}
use @ext_document.{Pos, require_editor_world}

fn cmd_goto_tagged_heading(kern: SystemKernel, _args: List) -> #InteractiveEffects !String Any {
    let world = require_editor_world(kern)!
    let buf = world.active_buffer()

    -- 第 1 步：收集当前 Markdown/Nessa 文件中的所有标题作为候选集
    let choices = List()
    for r in 0..buf.line_count() {
        let line = buf.line_at(r)
        if line.starts_with("#") or line.starts_with("-- ==") {
            choices.push((r + 1).to_string() ++ ": " ++ line.trim())
        }
    }

    -- 发起 Choice 提示符：此处 VM 自动捕获延续并挂起命令，绝不阻塞 GTK4 渲染！
    let picked? = ask_prompt(PromptRequest.choice("Jump to section: ", choices))# else {
        status_emit("Jump cancelled")#
        return ()
    }

    -- 第 2 步：甚至可以继续追问单键确认 (`y`/`n`)！
    let confirm? = ask_prompt(PromptRequest.confirm("Center viewport after jump? (y/n) "))# else {
        return ()
    }

    let colon_pair? = picked.split_once(":") else { return () }
    let (line_s, _) = colon_pair
    let row_1? = line_s.try_parse(i64) else { return () }
    buf.point = buf.clamp_pos(Pos.new(row_1 - 1, 0), insert_mode = false)
    if confirm == "y" {
        let _ = kern.dispatch_command("scroll-center", List())#!
    }
    status_emit("Jumped to line " ++ row_1.to_string())#
    ()
}
```

`PromptRequest` 开箱即用的便捷构造器包括：
- `PromptRequest.text(label, initial)`（自由文本输入）
- `PromptRequest.command(label, candidates)`（命令名，带快捷键反查徽章）
- `PromptRequest.file(label, initial, candidates)`（文件路径，带 `<C-l>` 父目录回退与权限大小徽章）
- `PromptRequest.buffer(label, candidates)`（缓冲区名，带 Consult 实时切换预览！）
- `PromptRequest.choice(label, candidates)`（自定义候选列表 Orderless 选择）
- `PromptRequest.confirm(label)`（`y`/`n` 单键即时确认）

---

## 挂载丰富 UI 装饰（侧边栏、浮窗、Gutter、虚拟文本与 Toast） *ui-decorations*

无需手写底层像素坐标，`EditorWorld` 为所有插件提供了声明式且按 `owner: String` 隔离管理的 6 类高层 UI 组件（由 `ext_render` 每帧自动合成）：

1. **悬浮窗口（`FloatingWindow`）**：调用 `world.set_floating_window(FloatingWindow { id, owner, title, anchor: "center" | "bottom" | "top-right", width_cols, height_rows, lines, selected_row, border_rgba })` 创建圆角描边弹窗（如 Telescope 与 ToggleTerm），用 `world.remove_floating_window(id)` 关闭。
2. **停靠侧边栏（`world.left_sidebar` / `world.right_sidebar`）**：设置 `SidebarPanel` 的 `visible = true`、`width_cols`、`title` 与 `lines`（如 Neo-tree 左栏文件树与 Aerial 右栏符号大纲），并监听 `"sidebar-clicked"` 钩子响应鼠标点击。
3. **行号左侧符号列（`GutterSign`）**：调用 `world.set_gutter_signs(buf_id, owner, signs)` 在指定行渲染 Git 增删色条、诊断图标或书签标记。
4. **行尾虚拟文本（`VirtualTextDecor`）**：调用 `world.set_virtual_texts(buf_id, owner, vtexts)` 在代码行末尾渲染不占真实字符偏移的辅助信息（如实时 Git Blame 作者时间或内联诊断提示）。
5. **行内背景/前景色块与下划线（`InlineSpanDecor`）**：调用 `world.set_inline_spans(buf_id, owner, spans)` 为任意行列区间着色（如真彩色 `#RRGGBB` 色块预览、TODO 胶囊徽章、光标下同词高亮与彩虹括号）。
6. **右上角非侵入通知卡片（`ToastNotification`）**：调用 `world.push_toast(owner, title, message, level, ttl_ticks)`（或直接调用 `ext_noice` 的 `:Notify` 命令）。

---

## 调用 `@lava_accel` 高性能原生算子 API *lava-accel-api*

当插件需要遍历成千上万个工作区文件、执行模糊排序、计算 Myers 行级 Diff 或检索全局文档时，只需在 `package.toml` 中添加 `"lava.org/lava_accel" = "0.1.0"` 即可调用由 Rust 多核加速（且自带纯 Nessa 兜底实现）的强类型函数：

| `@lava_accel` 公开函数 | 返回元素类型 | 典型插件用途 |
| --- | --- | --- |
| `accel_fuzzy_rank(query, candidates, .limit = 200)` | `List` of `FuzzyHit { index, score, positions }` | 自定义选择器或补全源的高性能模糊排序与命中位置提取 |
| `accel_workspace_files(root, .max_files = 5000, .include_hidden = false)` | `List` of `String`（相对路径） | 极速扫描项目文件树（自动遵守忽略规则） |
| `accel_workspace_grep(root, pattern, .case_sensitive = false, .max_results = 500)` | `List` of `GrepHit { file_path, row, col, line_text }` | 多线程并行全文代码搜索 |
| `accel_workspace_replace(root, search, replacement, ...)` | `(i64, i64)` `(files_changed, total_replacements)` | 跨文件批量原子替换 |
| `accel_diff_lines(old_lines, new_lines)` | `List` of `DiffOp { kind, old_start, old_count, new_start, new_count }` | 计算工作区缓冲区相对 `HEAD` 的最小增删改 Hunk |
| `accel_git_status(cwd)` / `accel_git_head_lines(cwd, rel_path)` / `accel_git_blame_line(cwd, rel_path, line_1based)` | `GitFileEntry` / `?List` / `?GitBlameInfo` | 驱动 Git 面板、文件树状态徽章与光标行 Blame |
| `accel_scan_symbols(lines, major_mode)` | `List` of `SymbolItem { name, kind, row, col, level }` | 提取当前缓冲区的函数/类型/章节大纲树 |
| `accel_scan_buffer_todos(lines)` / `accel_scan_workspace_todos(root)` | `List` of `TodoHit` | 扫描 `TODO`/`FIXME`/`NOTE`/`HACK`/`PERF`/`WARN`/`BUG` |
| `accel_index_docs(doc_roots)` / `accel_search_docs(doc_roots, query)` | `DocSectionEntry` / `DocSearchHit` | 解析 Markdown 手册 `*Info*` 节点与五域全文检索 |
| `accel_find_symbol_defs(root, symbol)` | `List` of `SymbolDefHit` | 在工作区源码中跨文件定位符号定义源文件与行号 |

---

## 完整端到端插件示例与无头验证 *complete-extension-example*

下面展示 `ext_wordcount/src/lib.ns` 的完整可编译实现：

```nessa
use @lava_core.{
    ExtId, InteractiveEffects, SystemKernel, log_event, status_emit
}
use @ext_document.{ext_document_id, require_editor_world}
use @ext_vim.{KEYMAP_VIM_LEADER, ext_vim_id}

pub fn ext_wordcount_id() -> ExtId = ExtId("user.local/ext_wordcount")

fn count_buffer_words(lines: List) -> (i64, i64) {
    var words: i64 = 0
    var chars: i64 = 0
    for i in 0..lines.len() {
        let ln = lines.get(i.as(usize)).as(String)
        chars += ln.char_len()
        words += ln.split_whitespace().len()
    }
    (words, chars)
}

pub fn register_wordcount_extension(kernel: SystemKernel) -> !String Unit {
    let ext_id = ext_wordcount_id()
    kernel.register_extension(
        ext_id,
        "user.local",
        "ext_wordcount",
        "0.1.0",
        "Count lines, words, and UTF-8 characters in the active buffer.",
        [ext_document_id(), ext_vim_id()],
        |k: SystemKernel| -> !String Unit {
            k.register_command(
                "word-count",
                ext_id,
                "Display line, word, and character statistics in the status bar.",
                |kern: SystemKernel, _args: List| -> #InteractiveEffects !String Any {
                    let world = require_editor_world(kern)!
                    let buf = world.active_buffer()
                    let (words, chars) = count_buffer_words(buf.lines)
                    let msg = buf.name ++ ": "
                        ++ buf.line_count().to_string() ++ " lines, "
                        ++ words.to_string() ++ " words, "
                        ++ chars.to_string() ++ " chars"
                    log_event("info", "ext_wordcount", msg)#
                    status_emit(msg)#
                    words
                },
            )
            let leader = k.ensure_keymap(KEYMAP_VIM_LEADER)
            leader.bind("<Space> c w", "word-count", ext_id, description = "Word count stats")
            ()
        },
        |_k: SystemKernel| -> !String Unit { () },
    )
}
```

保存后无需重新编译 Rust 宿主，直接通过 `--ext-dir` 校验并无头测试您的新插件：

```bash
# 1. 在单 VM 中静态类型检查新插件及全部依赖
./target/release/lava check --ext-dir /path/to/ext_wordcount

# 2. 在已发现扩展列表中确认 [user] user.local/ext_wordcount 已挂载
./target/release/lava list-extensions --ext-dir /path/to/ext_wordcount

# 3. 使用无头按键注入端到端运行 `<Space>cw` 命令
./target/release/lava --ext-dir /path/to/ext_wordcount README.md \
  --headless --keys "<Space>cw:q<Return>"
```
