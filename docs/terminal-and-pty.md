# 原生 Linux POSIX PTY 与 VT100/TrueColor 终端手册 *terminal*

Lava 不满足于只能回显静态命令输出的简易“伪命令行窗口”，而是由底层 Rust Crate **`crates/lava-term`** 与 Nessa 扩展 **`ext_toggleterm`**（发布服务 `"lava.toggleterm.manager"`）联合提供完整的 **原生 Linux POSIX 伪终端（PTY）+ VT100/ANSI/24-bit TrueColor 虚拟屏幕模拟器**。您可以直接在编辑器悬浮窗或任意分屏中运行交互式 `bash`/`zsh`/`fish`、`cargo watch`、`git log`、`less`，甚至嵌套运行 `htop` 或 `vim`，并随时按 `<C-\><C-n>` 将整个终端视口切换为可自由移动、搜索与复制的原生 Vim 缓冲区！返回手册总目录请参阅 `[index#top]`。

---

## 终端子系统架构：POSIX PTY + `VtScreen` 状态机 *pty* *vt100*

Lava 终端采用“Rust 零阻塞内核态 PTY 驱动 + Nessa 多会话窗口编排”的双层架构：

```text
[子进程 bash / zsh / htop / cargo]
        ▲                     │ stdout/stderr ANSI 字节流
        │ pty_send_key        ▼
[POSIX Master PTY fd] ──► [后台读取线程] ──► VtScreen (VT100 / 256色 / TrueColor / 备用屏幕)
                                                  │
                                                  ▼ 触发 UiEvent::Tick (0x50545900) 唤醒 VM
                                          [ext_toggleterm] 同步文本行、颜色 Span 与光标至浮窗/Buffer
```

### Linux 原生 POSIX PTY 进程管理 *posix-pty-engine*

`crates/lava-term` 直接通过系统调用创建真实终端设备：
- 调用 `openpty` 分配主从伪终端文件描述符，并通过 `TIOCSWINSZ` 按当前悬浮窗或分屏窗格的精确行列数设置初始窗口尺寸（当您调整分屏大小或缩放 GTK4 窗口时，`sync_terminal_buffer_with_wm` 自动调用 `pty_resize` 向子进程发送 `SIGWINCH`！）；
- `fork()` 后在子进程中调用 `setsid()` 建立独立会话，以 `TIOCSCTTY` 绑定控制终端，注入 `TERM=xterm-256color` 与 `COLORTERM=truecolor` 环境变量，并在指定工作目录（`cwd`）启动您的 `$SHELL`（或自定义命令）；
- 独立后台读取线程持续非阻塞消费主设备输出，每当屏幕状态更新便投递专用定时器唤醒事件 `PTY_WAKE_TIMER_ID`（`0x5054_5900`），使闲置挂起的 Nessa VM 零轮询即时刷新画面。
- **无头测试平滑降级**：在不支持宿主 PTY 的纯 `nessa run` 独立单元测试环境下，`ext_toggleterm` 自动透明降级为带状态记忆（支持跨命令 `cd`、`pwd`、`clear` 与历史栈）的命令行回退引擎，保证测试 100% 确定性可复现。

### VT100 / ANSI / TrueColor 解析特性 *vt100-features*

内置 `VtScreen` 状态机完整支持现代 CLI 与 TUI 工具所需的核心控制序列：
- **色彩渲染**：标准 ANSI 16 色（含高亮前景 `90-97` / 背景 `100-107`）、xterm 256 色调色板（`ESC[38;5;Nm` / `48;5;Nm`）以及 **24-bit RGB 真彩色**（`ESC[38;2;R;G;Bm` / `48;2;R;G;Bm`），以及粗体（Bold）、斜体（Italic）、反显（Inverse）属性段。
- **光标与擦除指令**：`CUU`/`CUD`/`CUF`/`CUB`/`CUP` 绝对与相对光标定位、`ED`/`EL` 屏幕与行擦除、`IL`/`DL`/`ICH`/`DCH` 行列插入删除、`DECSTBM`（`ESC[top;bottomr`）局部滚动区域、光标显隐（`?25h` / `?25l`）与应用光标键模式（`DECCKM` `?1h`）。
- **主屏幕滚回与备用屏幕（Alternate Screen）**：主屏幕维护多达数千行的历史滚回缓冲区（Scrollback）；当运行 `htop`、`less`、`vim` 等全屏程序发送 `ESC[?1049h`（或 `?47h`/`?1047h`）时，自动切入独立备用屏幕，退出程序发送 `ESC[?1049l` 后毫秒级原样恢复之前的 Shell 命令行与滚回历史！
- **OSC 协议集成**：实时解析 `OSC 0` / `OSC 2` 动态更新终端标题，解析 `OSC 7`（`file://host/path`）实时感知 Shell 内部 `cd` 后的最新工作目录。

---

## 四种终端窗口布局与打开命令 *toggleterm*

`ext_toggleterm` 支持同时维护最多 **`1..9` 个独立编号的持久化终端会话**（命名为 `*terminal-1*` .. `*terminal-9*`）。同一个编号会话可以在以下四种布局间随时切换而绝不丢失正在运行的进程：

| 布局形态 | 快捷键 | Ex 命令 / M-x 命令 | 适用场景 |
| --- | --- | --- | --- |
| **居中悬浮终端（Float）** | **`<C-\>`**（Normal & Insert 均可用）、`<M-\>`、`<Space> t f` | `:ToggleTerm [id_or_cmd]` / `M-x terminal-float` | 默认 `88x22` 居中圆角浮窗：随时按 `<C-\>` 秒开查状态或跑单测，再按 `<C-\>` 秒关回到代码处 |
| **水平底部分屏（Split）** | `<Space> t h` | `:TermSplit [id_or_cmd]` / `M-x terminal-split` | 在当前窗格下方切出终端分屏，边看上方源码边观察编译/服务日志 |
| **垂直右侧分屏（VSplit）** | `<Space> t v` | `:TermVSplit [id_or_cmd]` / `M-x terminal-vsplit` | 宽屏显示器左右并排对照编码与 REPL 交互 |
| **全窗格缓冲区（Buffer）** | `<Space> t t` / `<Space> t b` | `:term [cmd]` / `:terminal [cmd]` / `:vterm [cmd]` / `:ToggleTermBuffer [id]` | 将当前整个活动窗格切换为终端缓冲区（在 Dirvish 中按 `S` 也会调用 `terminal-here` 在当前目录打开） |

### 多实例会话管理（`1..9`） *multi-session-management*

- **按编号直接瞬移**：在任何终端处于 `TERM-INSERT` 模式（含悬浮窗与分屏）时，直接按 **`<M-1>` .. `<M-9>`** 立即在原地切换（或按需创建）第 `1..9` 号终端会话！
- **前后循环切换**：按 `]t` / `[t`（`toggleterm-next` / `toggleterm-prev`），或在 `TERM-NORMAL` 模式下按 `<Tab>` 循环切换已有会话。
- **指定命令运行**：执行 `:ToggleTerm htop`、`:TermSplit cargo test` 或按 `<Space> t r`（`:TermExec <cmd>`）可直接在活动终端中投递并运行指定命令。

---

## `TERM-INSERT` 与 `TERM-NORMAL` 双模态协同 *term-modes*

在缓冲区/分屏终端（以及悬浮终端）中，状态栏左侧会清晰显示当前终端徽章（如 `[TERM-INSERT #1]` 或 `[TERM-NORMAL #1]`）：

### `TERM-INSERT` 模式：原生按键全量直通 *term-insert-mode*

打开终端时默认进入 `TERM-INSERT` 模式：
- `ext_toggleterm` 挂载模态拦截器，将您敲击的所有字母、`<Return>`、`<Tab>`（触发 bash/zsh 原生补全）、`<Up>`/`<Down>`（Shell 历史）、`<C-c>`（发送 `SIGINT` 中断信号）、`<C-d>`（EOF）、`<C-r>`（Shell 反向历史搜索）、`<C-l>`（清屏）直接编码送入 POSIX PTY 主设备！
- **悬浮窗快捷隐藏**：在悬浮终端处于 `TERM-INSERT` 时，直接按一次 **`<C-\>`**（或 `<M-\>`）即可瞬间隐藏浮窗，进程在后台继续全速执行。

### `TERM-NORMAL` 模式：把终端输出变成 Vim 缓冲区 *term-normal-mode*

当终端输出了大段编译报错栈、文件路径或 JSON 数据，您无需伸手抓鼠标框选复制！

1. **如何从 `TERM-INSERT` 切入 `TERM-NORMAL`**：
   - 按经典 Neovim 终端逃逸序列 **`<C-\><C-n>`**（按下 `<C-\>` 时状态栏会提示 `<C-\>- (press <C-n> for TERM-NORMAL, <C-\> for float)`，紧跟 `<C-n>` 或 `n` 即可）；
   - 或者一键按 **`<F7>`** 或 **`<M-Escape>`**（也可执行 `:TermNormal`）。
2. **在 `TERM-NORMAL` 中能做什么**：
   - 拦截器自动释放控制权，**整套 Evil-Vim 导航与复制能力原地满血生效**！
   - 使用 `h`/`j`/`k`/`l`、`w`/`b`、`0`/`$`、`gg`/`G`、`<C-u>`/`<C-d>` 自由上下翻阅数千行历史滚回输出；
   - 使用 `/` 或 `?` 搜索错误关键词并用 `n`/`N` 跳转，或按 `gs` 用 Flash 标签直飞任意屏幕词；
   - 使用 `v` / `V` / `<C-v>` 选中任意输出片段并按 `y` 复制进系统剪贴板，切回代码窗格直接按 `p` 粘贴！
   - 按 `<C-w> h/j/k/l` 自由切换到旁边的代码分屏窗格。
3. **如何从 `TERM-NORMAL` 重新回到 `TERM-INSERT`**：
   - 在终端缓冲区中按 **`i`**、**`a`**、**`I`**、**`A`** 或 **`<Return>`** 任意一键，立即无缝切回 `[TERM-INSERT]` 继续向 Shell 输入命令；
   - 按 **`q`** 则隐藏或切离当前终端视图。

---

## 将编辑器代码行或选区一键投递至终端（REPL 工作流） *termsend-workflow*

编写 Shell 脚本、Python、SQL 或 CLI 命令草稿时，您可以直接把编辑器缓冲区中的文本流式发送到后台存活的 PTY 终端执行（自动补换行符并同步刷新终端快照）：

- **发送当前行（Normal 模式）**：将光标停在任意一行命令上，按 **`<Space> t s`**（`terminal-send-line`）或执行 `:TermSend`。
- **发送多行选区（Visual 模式）**：按 `v` 或 `V` 框选一段多行脚本后，按 **`<Space> t s`** 或执行 `:'<,'>TermSend`，整段选区按序直达当前激活编号终端的标准输入！
- **直接发送指定参数命令**：执行 `:TermSend cargo check --workspace`，无需切换窗口焦点即可让后台终端立即开跑。
