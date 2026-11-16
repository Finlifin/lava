# Evil-Vim 与 Emacs 混合键位权威参考 *keybindings*

Lava 原生融合了完整的 **Evil-Vim 模态状态机**（`ext_vim`）与 **GNU Emacs 命令/前缀键/Kill-Ring 体系**（`ext_emacs`），并通过 `<Space>` Leader 键位树将 40 个内置扩展无缝串联。两者并非互斥补丁：在 Evil Normal 和 Insert 模式下，未被模态绑定的 `M-x`、`C-x`、`C-h`、`C-s`、`C-g`、`M-%` 自动透传至 Emacs 引擎；而按下 `<C-z>` 更可在模态与纯 Emacs 无模态之间一键切换。返回手册总目录请参阅 `[index#top]`。

---

## Evil 六大编辑模式总览 *evil-modes*

| 模式名称 | 状态栏徽章 | 光标形状 | 进入方式 | 退出方式 |
| --- | --- | --- | --- | --- |
| **Normal（普通模式）** | `NORMAL` | `Block`（实心块） | 默认模式；在任意其他模式按 `<Escape>` 或 `<C-[>` | 进入其他模式或按 `<C-z>` 切换至 `EMACS` |
| **Insert（插入模式）** | `INSERT` | `Beam`（细竖线） | `i`（光标前）、`I`（行首非空字符前）、`a`（光标后）、`A`（行尾）、`o`（下方新行）、`O`（上方新行）、`s`/`S`/`c`/`C` | `<Escape>`、`<C-[>` 或 `<C-c>` |
| **Visual Char（字符可视）** | `VISUAL` | `Block` + 字符选区 | `v`（在 Visual Line / Block 中按 `v` 可保留锚点无缝切换） | `<Escape>`、再次按 `v` 或执行操作符 |
| **Visual Line（整行可视）** | `V-LINE` | `Block` + 整行选区 | `V` | `<Escape>`、再次按 `V` 或执行操作符 |
| **Visual Block（矩形块可视）** | `V-BLOCK` | `Block` + 矩形选区 | `<C-v>`（支持 `$` 跟随各行行尾，以及 `I` / `A` 多行同步块插入！） | `<Escape>`、再次按 `<C-v>` 或执行操作符 |
| **Replace（覆盖替换模式）** | `REPLACE` | `Underline`（下划线） | `R`（连续覆盖字符；按 `<BackSpace>` 会从覆盖栈精确恢复原字符！） | `<Escape>` 或 `<C-[>` |
| **Operator-Pending（等待动作）** | `OP-PENDING` | `Block` | 在 Normal 模式下按下 `d`、`c`、`y`、`>`、`<`、`gu`、`gU`、`g~` 后等待动作或文本对象 | 完成动作/文本对象，或按 `<Escape>` 取消 |

> **计数前缀（Counts）**：Evil 完整支持 `[count1] operator [count2] motion` 乘法语义——例如 `2d3w` 等价于删除 `2 * 3 = 6` 个词，且计数与目标寄存器完整录制进 `.`（点重复）事务！

---

## Evil 光标移动（Motions）全表 *evil-motions*

以下所有移动指令均同时适用于 **Normal**、**Visual（含矩形块）** 与 **Operator-Pending** 三大上下文：

### 基础方向与行内移动 *motions-basic-line*

| 按键 | 移动形状 | 功能说明 |
| --- | --- | --- |
| `h` / `<Left>` / `<BackSpace>` | Exclusive | 向左移动 `N` 列 |
| `j` / `<Down>` | Linewise | 向下移动 `N` 行（记忆期望列 `preferred_col`） |
| `k` / `<Up>` | Linewise | 向上移动 `N` 行（记忆期望列 `preferred_col`） |
| `l` / `<Right>` | Exclusive | 向右移动 `N` 列 |
| `0` | Exclusive | 跳至当前行第 0 列（数字输入中途的 `0` 作为十进制数位） |
| `^` | Exclusive | 跳至当前行首个非空白字符 |
| `$` | Inclusive | 跳至当前行（或向下 `N-1` 行）末尾；在 `<C-v>` 矩形块中激活行尾跟随模式 |
| `g_` | Inclusive | 跳至当前行（或向下 `N-1` 行）最后一个非空白字符 |

### 单词、字符查找与结构移动 *motions-word-char-struct*

| 按键 | 功能说明 |
| --- | --- |
| `w` / `W` | 正向跳至下 `N` 个单词（`w`）或空白分隔大词（`W`）词首（注：`cw` / `cW` 遵循经典 Vim 规则不吞尾部空格，等价于 `ce` / `cE`） |
| `b` / `B` | 反向跳至上 `N` 个单词（`b`）或大词（`B`）词首 |
| `e` / `E` | 正向跳至第 `N` 个单词（`e`）或大词（`E`）词尾（Inclusive） |
| `ge` / `gE` | 反向跳至上 `N` 个单词（`ge`）或大词（`gE`）词尾（Inclusive） |
| `f<ch>` / `F<ch>` | 在当前行正向 / 反向查找第 `N` 个字符 `<ch>` 并停在其上 |
| `t<ch>` / `T<ch>` | 在当前行正向 / 反向查找第 `N` 个字符 `<ch>` 并停在其前一列 |
| `;` / `,` | 按同方向（`;`）或反方向（`,`）重复最近一次 `f`/`F`/`t`/`T` 行内查找 |
| `{` / `}` | 向上 / 向下跳转 `N` 个空行分隔的段落边界 |
| `%` | 在当前行或光标处的 `()`、`[]`、`{}` 配对括号之间精确嵌套跳转 |

### 全文、视口、搜索与跳转表移动 *motions-jump-scroll-search*

| 按键 | 功能说明 |
| --- | --- |
| `gg` / `[N]gg` | 跳至缓冲区首行（或第 `N` 行）首个非空白字符（记录 JumpList） |
| `G` / `[N]G` | 跳至缓冲区末行（或第 `N` 行）首个非空白字符（记录 JumpList） |
| `H` / `M` / `L` | 跳至当前可视视口顶部（High）、中部（Middle）、底部（Low）行 |
| `<C-d>` / `<C-u>` | 向下 / 向上半屏滚动并保持光标相对视口推进 |
| `<C-f>` / `<C-b>` | 向下 / 向上整屏翻页滚动 |
| `zz` / `zt` / `zb` | 保持光标行列不变，将光标所在行滚动至视口正中 / 顶部 / 底部 |
| `/` / `?` | 正向 / 反向增量搜索（支持定界延续 Minibuffer 实时 `incsearch` 预览与 `<C-g>` 回滚） |
| `*` / `#` | 正向 / 反向精确整词搜索光标下的标识符 |
| `n` / `N` | 按原方向 / 反方向跳转至下一处搜索匹配 |
| `<C-o>` / `<C-i>` (`<Tab>`) | 在跨缓冲区跳转历史 `JumpList` 中后退 / 前进（自动跨文件打开或切换 Buffer） |
| `gs` / `gS` | **Flash 2D 闪电跳转**（`ext_flash`）：输入搜索前缀后按单字符无冲突标签直飞屏幕任意位置（`gs`）或选择外层括号节点（`gS`） |

### 括号对 `[` / `]` 快速导航族 *motions-bracket-pairs*

| 组合键 | 所属扩展 | 功能说明 |
| --- | --- | --- |
| `]q` / `[q` | `ext_vim` | 跳至 `*Quickfix*` 下一条 / 上一条条目 |
| `]d` / `[d` | `ext_trouble` | 跳至下一处 / 上一处缓冲区诊断（Diagnostics） |
| `]c` / `[c` | `ext_gitsigns` | 跳至下一处 / 上一处 Git 修改块（Hunk） |
| `]x` / `[x` | `ext_git_conflict` | 跳至下一处 / 上一处 Git 合并冲突区域 |
| `]r` / `[r` | `ext_illuminate` | 跳至光标下符号的下一处 / 上一处同词引用 |
| `]t` / `[t` | `ext_todo_comments` / `ext_toggleterm` | 跳至下一处 TODO 注释或切换终端会话 |
| `[y` / `]y` | `ext_yanky` | 在刚执行完 `p`/`P` 粘贴后，原地将粘贴内容循环替换为历史剪贴环中的更早 / 更新项 |

---

## Evil 操作符、单键编辑与插件操作符 *evil-operators*

| 按键 | 类别 | 功能说明 |
| --- | --- | --- |
| `d{motion}` / `dd` / `D` | 操作符 | 删除动作覆盖范围 / 删除 `N` 整行 / 删除光标至行尾（写入寄存器与 `KillRing`） |
| `c{motion}` / `cc` / `C` | 操作符 | 修改动作范围并进入 Insert / 修改整行（保留缩进） / 修改至行尾 |
| `y{motion}` / `yy` / `Y` | 操作符 | 复制动作范围 / 复制 `N` 整行（触发 `"vim-yank"` 钩子并短暂高亮复制区域） |
| `>{motion}` / `>>` | 操作符 | 向右增加一级缩进（按 `shiftwidth` 与 `expandtab` 规则） |
| `<{motion}` / `<<` | 操作符 | 向左减少一级缩进 |
| `gu{motion}` / `gugu` | 操作符 | 转为全小写（Visual 模式下直接按 `u`） |
| `gU{motion}` / `gUgU` | 操作符 | 转为全大写（Visual 模式下直接按 `U`） |
| `g~{motion}` / `g~g~` / `~` | 操作符 | 翻转大小写（Normal 下 `~` 翻转光标起 `N` 个字符） |
| `x` / `X` / `s` / `S` | 单键编辑 | 删除当前字符 / 向前删字符 / 替换字符进 Insert / 替换整行进 Insert |
| `r<ch>` | 单键编辑 | 将光标起 `N` 个字符（或整个 Visual 选区，支持 `r<Return>`）原地替换为 `<ch>` |
| `J` | 单键编辑 | 将当前行与下一行合并（自动以单空格衔接） |
| `p` / `P` | 粘贴 | 在光标后（或下方行）/ 光标前（或上方行）粘贴寄存器内容（自动识别字符级、整行级与 `<C-v>` 矩形块级！） |
| `.` | 点重复 | 精确重放上一次完整修改事务（包括 Operator + 文本对象 + Insert 输入全过程，支持 `[N].` 覆盖计数） |
| `gcc` / `gc{vis}` / `gbc` | `ext_comment` | 切换当前行行注释 / Visual 选区行注释 / 块注释（自动识别 `--`、`//`、`#`、`/* */`、`<!-- -->`） |
| `ys{motion}<ch>` / `ds<ch>` / `cs<old><new>` | `ext_surround` | 添加包裹符（Visual 下按 `S<ch>`）、删除外层包裹符、修改包裹符（支持 `)`/`(` 带内边距差异及 `t` HTML 标签！） |
| `<C-a>` / `g<C-x>` | `ext_treesj` | 智能递增 / 递减光标下整数、十六进制数、`true`/`false`、`yes`/`no` 等布尔词 |
| `<M-j>` / `<M-k>` | `ext_treesj` | 将当前行或整个 Visual 选区向下 / 向上整块平移 |
| `<C-n>` | `ext_multicursor` | 选中当前词并逐次向下添加下一个同词多光标（支持多光标同步 `i`/`a`/`c`/`d` 编辑，`<Escape>` 退出） |

---

## 嵌套文本对象（Text Objects） *evil-text-objects*

在 **Operator-Pending**（如 `diw`、`ci"`、`va(`）或 **Visual** 模式下按 `i`（Inner，内部不含边界）或 `a`（Around，外部含边界/邻近空白）即可选中语义范围：

| 文本对象键 | `i`（内部范围） | `a`（外部范围） |
| --- | --- | --- |
| `iw` / `aw` | 当前单词（标识符或标点串） | 当前单词及其一侧相连空白 |
| `iW` / `aW` | 当前非空白连续大词（WORD） | 当前大词及其相邻空白 |
| `ip` / `ap` | 当前连续非空行段落 | 当前段落及其下方紧邻的空行块 |
| `i"` / `a"` | 双引号 `"..."` 内部文本（处理 `\"` 转义） | 包含两侧双引号本身 |
| `i'` / `a'` | 单引号 `'...'` 内部文本 | 包含两侧单引号本身 |
| `` i` `` / `` a` `` | 反引号 `` `...` `` 内部文本 | 包含两侧反引号本身 |
| `i(` / `i)` / `ib` 与 `a(` / `a)` / `ab` | 圆括号 `(...)` 内部（支持跨多行嵌套配对与 `[N]i(` 外扩层级） | 包含两侧 `(` 与 `)` |
| `i[` / `i]` 与 `a[` / `a]` | 方括号 `[...]` 内部（跨行嵌套感知） | 包含两侧 `[` 与 `]` |
| `i{` / `i}` / `iB` 与 `a{` / `a}` / `aB` | 花括号 `{...}` 内部（跨行嵌套感知） | 包含两侧 `{` 与 `}` |
| `i<` / `i>` 与 `a<` / `a>` | 尖括号 `<...>` 内部 | 包含两侧 `<` 与 `>` |

> **Visual Block 多行同步插入技巧**：按 `<C-v>` 框选多行矩形列后，按大写 `I`（块前插入）或 `A`（块后插入），输入任意前缀/后缀文本后按 `<Escape>`，Lava 会自动将该文本一次性同步应用到矩形选区的所有行，并可通过单次 `u` 原子撤销！

---

## 寄存器、标记、键盘宏与分叉撤销树 *registers-marks-macros-undotree*

### 双模统一寄存器银行（Registers） *evil-registers*

通过 `"<reg>` 前缀指定读写寄存器（执行 `:reg` 或 `:registers` 查看所有非空寄存器）：

- `""`（无名寄存器）：保存最近一次删除或复制；当 `clipboard_unnamedplus = true`（默认）时自动读写系统剪贴板。
- `"+` / `"*`：显式读写系统剪贴板（通过 `clipboard_get` / `clipboard_set` 代数效应桥接）。
- `"_`：黑洞寄存器（`"_dd` 删除文本但绝不污染任何寄存器或剪贴板）。
- `"0`：专用复制（Yank）寄存器，仅被 `y` 更新，不受后续 `d`/`c`/`x` 覆盖。
- `"1` .. `"9`：整行删除滚动历史栈（每次整行删除压入 `"1`，旧内容依次顺延至 `"9`）。
- `"-`：行内短文本删除寄存器。
- `"a` .. `"z` 与 `"A` .. `"Z`：26 个具名寄存器；使用大写字母（如 `"Ayy`）会将内容**追加**到原寄存器末尾！

### 局部与跨文件全局标记（Marks） *evil-marks*

执行 `:marks` 可列出所有已设标记：

- `ma` .. `mz`：设置当前缓冲区局部标记；`'a` 跳至标记所在行首个非空字符，`` `a `` 精确跳至标记行列。
- `mA` .. `mZ`：设置**全局跨文件标记**（记录 `BufferId`、磁盘路径与行列）；在任意缓冲区按 `'A` 或 `` `A `` 会自动打开或切回对应文件！
- `''` / `` `` ``：跳回起跳前的位置；`'<` / `'>`：最近一次 Visual 选区首尾；`'.`：最近一次修改位置。

### 键盘宏（Keyboard Macros） *evil-macros*

- `q<a-z>`：开始向指定字母寄存器录制按键流（状态栏显示 `recording @a`）；录制期间再次按 `q` 停止录制。
- `@<a-z>` / `[N]@a`：回放宏 `N` 次（最大递归深度保护为 16 层）；`@@` 重放最近一次执行过的宏。
- Emacs 风格键盘宏同样可用：`C-x (` 开始录制、`C-x )` 结束录制、`C-x e` 结束并立即回放（支持 `C-u <N> C-x e`）。

### 分叉撤销树（Branching UndoTree） *branching-undotree*

Lava 的每个缓冲区内置非线性的 **分叉 `UndoTree`**，绝不会像传统线性撤销栈那样在“撤销后重新编辑”时丢失旧时间线：

- `u` / `C-_` / `C-x u`：沿当前活动分支向父节点撤销 `N` 步。
- `<C-r>`：沿当前选定的子节点重做 `N` 步。
- `g-` / `g+`：在同一个父状态分叉出的多个兄弟历史分支之间切换！
- `<Space> u` / `:undotree`：打开可视化 `*UndoTree*` 缓冲区查看完整树状快照节点图。

---

## `:Ex` 命令完整速查表 *ex-commands*

在 Normal 模式按 `:`（或在 Visual 模式按 `:` 自动填入 `'<,'>` 范围）打开具备两阶段补全的 Ex 提示符。支持标准行范围语法：`.`（当前行）、`$`（末行）、`%`（全文 `1,$`）、`N,M`（显式行号）、`'a,'b`（标记区间）以及 `'<,'>`（Visual 选区）：

| Ex 命令与缩写 | 功能说明 |
| --- | --- |
| `:e [path]` / `:edit` / `:e!` | 打开文件；`:e!` 丢弃未保存修改从磁盘重新加载当前文件 |
| `:w [path]` / `:[range]w <path>` | 保存当前缓冲区（支持保存指定行范围或另存为新路径；在 `oil`/`spectre` 中提交变更） |
| `:q` / `:quit` / `:q!` / `:qa!` | 关闭当前窗格或退出编辑器（有未保存修改时拒绝，加 `!` 强制丢弃修改退出） |
| `:wq [path]` / `:x` / `:xit` | 保存并退出 |
| `:b <name_or_id>` / `:bn` / `:bp` / `:bd[!]` / `:ls` | 切换缓冲区、前后轮换、关闭缓冲区、打开 `*Buffers*` 列表 |
| `:sp [file]` / `:vsp [file]` / `:only` / `:close` | 水平分屏、垂直分屏、仅保留当前窗格、关闭当前窗格 |
| `:tabnew [file]` / `:tabn` / `:tabp` / `:tabc` | 新建标签页、切换下/上一标签页、关闭标签页 |
| `:[range]s/pat/rep/[gi]` | 行范围正则/模式替换（默认全文或指定范围；`g` 替换同行全部匹配，`i` 忽略大小写） |
| `:[range]g/pat/cmd` / `:[range]v/pat/cmd` | 对匹配（`g`）或不匹配（`v`）`pat` 的所有行批处理执行 `d`（删除）、`y`（复制）或 `s/...` |
| `:[range]d` / `:[range]y` / `:<N>` | 删除指定行范围、复制指定行范围、直接跳转到第 `N` 行 |
| `:set [opts...]` / `:noh` (`:nohlsearch`) | 修改 `EditorOptions` 选项（见 `[user-guide#top]` 与 `[Info: *options*]`）；临时关闭当前搜索高亮 |
| `:colorscheme <name>` / `:colo` / `:theme` | 切换配色方案（带实时全窗预览） |
| `:cd [dir]` / `:pwd` | 切换工作区当前目录 / 在状态栏显示当前工作目录 |
| `:reg` (`:registers`) / `:marks` / `:undotree` / `:ext` | 打开 `*Registers*`、`*Marks*`、`*UndoTree*`、`*Extensions*` 检查缓冲区 |
| `:Dirvish [dir]` / `:Ranger [dir]` / `:Ex` / `:Oil` | 打开 Dirvish 三栏文件管理器（见 `[dirvish-and-ranger#top]`）或 Oil 可编辑目录缓冲区 |
| `:Term [cmd]` / `:ToggleTerm [id]` / `:TermSplit` / `:TermVSplit` / `:TermSend` | 打开或控制 POSIX PTY 终端会话（见 `[terminal-and-pty#top]`） |
| `:Telescope [picker] [query]` | 打开模糊检索器（`files`/`live_grep`/`buffers`/`symbols`/`keymaps`/`colorscheme`/`help_tags`/`docs`/`resume`） |
| `:h [topic]` / `:help [topic]` / `:Info [node]` / `:Apropos <q>` / `:AproposDocs <q>` | 打开 Emacs 帮助与 `*Info*` 超文本文档系统（见 `[emacs-help-and-info#top]`） |
| `:Lazy` / `:CheckHealth` | 打开单 VM 插件管理器或运行系统健康自检 |
| `:<command-name> [args...]` | **通用桥接**：任何注册在 `SystemKernel` 中的命令名均可直接在 `:` 提示符调用！ |

---

## Emacs 组合键与无模态编辑参考 *emacs-chords*

### 跨模式全局可用的 Emacs 核心键位 *emacs-global-chords*

无论 `evil_mode` 开启与否，以下经典 Emacs 组合键在日常编辑中随时可用：

| 组合键 | 绑定命令 | 功能说明 |
| --- | --- | --- |
| `<M-x>` | `execute-extended-command` | 呼出命令面板（带 Marginalia 快捷键提示与 Orderless 过滤） |
| `<C-g>` | `keyboard-quit` | 取消当前输入前缀、关闭激活提示符并回滚预览、停用选区 |
| `<C-u>` | `universal-argument` | 通用前缀参数（一次 `C-u` = 4，两次 = 16，或 `C-u 12` 传递精确计数 `12`） |
| `<C-z>` | `toggle-evil-mode` | 在 Evil-Vim 模态编辑与纯 Emacs 无模态编辑之间即时切换 |
| `<C-s>` / `<C-r>` | `isearch-forward` / `backward` | 增量正向/反向搜索（在提示符内连按 `C-s` / `C-r` 直接跳向下一处/上一处命中！） |
| `<M-%>` | `query-replace` | 基于定界延续的交互式逐项查找替换（每处命中按 `y` 替换、`n` 跳过、`!` 全部替换、`q` 退出） |
| `<M-!>` / `<M-&>` | `shell-command` / `async-shell-command` | 执行同步或异步后台外部 Shell 命令并将输出展示在缓冲区 |
| `<M-:>` / `C-x C-e` | `eval-expression` / `eval-scratch-line` | 在 `*scratch*` 实时求值 Nessa 表达式并内联写回结果 |
| `C-x C-f` / `C-x C-s` / `C-x C-w` / `C-x s` | 文件命令族 | 打开文件、保存当前文件、另存为、询问保存所有已修改缓冲区 |
| `C-x b` / `C-x k` / `C-x C-b` / `C-x C-c` | 缓冲区与退出 | 切换缓冲区、关闭缓冲区、打开 `*Buffers*` 列表、安全退出编辑器 |
| `C-x 2` / `C-x 3` / `C-x 0` / `C-x 1` / `C-x o` | 窗口命令族 | 上下分屏、左右分屏、关闭当前窗格、仅留当前窗格、切换窗格焦点 |
| `C-x d` / `C-x g` | 工具入口 | 打开 Dirvish（或 Dired）目录浏览器 / 打开 `*magit*` Git 面板 |
| `C-x n n` / `C-x n w` | `narrow-to-region` / `widen` | 将缓冲区可见可编辑范围窄化至当前选区 / 恢复显示完整缓冲区 |
| `C-x r k` / `y` / `t` / `d` | 矩形块（Rectangle）命令族 | 剪切矩形区域、粘贴矩形块、为每行替换矩形前缀字符串、删除矩形块 |
| `C-x (` / `C-x )` / `C-x e` | 键盘宏命令族 | 开始录制 Emacs 宏、结束录制、结束并回放宏 |
| `C-h ?` / `f` / `v` / `k` / `m` / `w` / `a` / `o` / `d` / `i` / `p` | Emacs 自省与帮助族 | 完整全局文档检索与源码自省套件（详见 `[emacs-help-and-info#top]`） |

### 纯 Emacs 无模态（`evil_mode = false`）专属移动与 Kill-Ring 键位 *emacs-modeless-keys*

当按 `<C-z>` 切换至 `EMACS` 模式后，所有字母键直接插入字符，并启用整套 GNU Emacs 移动与 60 槽位 `KillRing` 编辑键：

- **光标移动**：`C-f` / `C-b`（前/后字符）、`C-n` / `C-p`（下/上一行）、`C-a` / `C-e`（行首/行尾）、`M-f` / `M-b`（前/后一词）、`M-<` / `M->`（全文首/尾，自动压入 Mark）、`C-v` / `M-v`（下/上翻页）、`C-l`（视口居中重绘）。
- **选区与剪切环（Kill-Ring）**：`C-Space` 或 `C-@`（`set-mark-command` 激活选区）、`C-x C-x`（交换 Point 与 Mark）、`C-x h`（全选缓冲区）、`C-d`（向后删字）、`M-d` / `M-BackSpace`（正向/反向删词入环）、`C-k`（剪切至行尾，连续 kill 自动合并入同一槽位！）、`C-w`（剪切选区）、`M-w`（复制选区）、`C-y`（粘贴最新槽位）、紧跟 `M-y`（`yank-pop` 原地循环轮换历史剪切条目！）。

---

## `<Space>` Leader 快捷键全表（实时 Which-Key 引导） *leader-keys*

在 Evil Normal 模式下按下 `<Space>`，窗口底部会自动弹出 **Which-Key 候选提示栏**，按功能域前缀分组展示所有内置与用户绑定：

| Leader 序列 | 触发命令 | 所属扩展与功能说明 |
| --- | --- | --- |
| `<Space> f f` / `<Space> f s` / `<Space> f g` | `find-file` / `save-buffer` / `grep` | 打开文件、保存当前文件、启动异步工作区 Grep |
| `<Space> f d` / `<Space> R` / `<Space> f o` | `dirvish` / `oil-open-parent` | 打开 Dirvish 三栏文件管理器 / 打开 Oil 可编辑目录缓冲区 |
| `<Space> e` / `<Space> n t` / `<Space> n f` / `<Space> n r` | `dired` / `neotree-*` | 打开 Dired / 切换左侧 Neo-tree 文件树、聚焦文件树、在树中定位当前文件 |
| `<Space> b b` | `switch-to-buffer` | Minibuffer 切换缓冲区（带实时内容预览） |
| `<Space> w v` / `<Space> w s` / `<Space> w d` | `split-window-right` / `below` / `delete-window` | 垂直分屏、水平分屏、关闭当前分屏窗格 |
| `<Space> s f` / `s g` / `s b` / `s s` / `s k` / `s c` / `s r` | `telescope-*` | Telescope：找文件、实时 Grep、切 Buffer、符号大纲、键位查询、配色预览、恢复上次检索 |
| `<Space> f h` / `<Space> s H` | `telescope-help-tags` / `telescope-docs` | Telescope：搜索帮助标签 / 全文搜索 Lava & Nessa 文档 |
| `<Space> s R` / `<Space> s t` / `<Space> s n` / `<Space> s d` | `spectre-open` / `todo-quickfix` / `notifications` / `dashboard` | 打开 Spectre 全局替换面板、汇总 TODO、查看历史通知、打开启动页 |
| `<Space> h a` / `<Space> h m` / `<Space> 1`..`4` | `harpoon-*` | 将当前文件加入 Harpoon 书签、打开书签浮窗、一键瞬移第 1~4 号核心文件 |
| `<Space> g g` / `<Space> g b` | `magit-status` / `gitsigns-toggle-blame` | 打开 `*magit*` 面板 / 切换行尾实时 Git Blame 虚拟文本 |
| `<Space> h s` / `<Space> h r` / `<Space> h p` | `gitsigns-stage-hunk` / `reset` / `preview` | 暂存当前 Git Hunk、回滚当前 Hunk、浮窗预览当前 Hunk Diff |
| `<Space> c o` / `c t` / `c b` / `c 0` / `c q` | `git-conflict-*` | 一键解决合并冲突（保留 Ours / Theirs / Both / None / 导出全部冲突至 Quickfix） |
| `<Space> c f` / `<Space> c s` / `<Space> x x` | `conform-format` / `aerial-toggle` / `trouble-toggle` | 格式化当前代码、开关右侧符号大纲栏、开关 `*Trouble*` 诊断面板 |
| `<Space> /` / `<Space> j` / `<Space> p` | `comment-toggle-line` / `treesj-toggle` / `yanky-history` | 切换行注释、单行/多行括号结构拆分合并（TreeSJ）、打开剪贴环历史选择器 |
| `<Space> m a` / `<Space> m j` / `<Space> m k` | `mc-add-*` | 选中全文所有同词创建多光标 / 向下或向上添加垂直列多光标 |
| `<Space> t f` / `t t` / `t h` / `t v` / `t b` / `t r` / `t s` | `ext_toggleterm` 终端族 | 悬浮终端、全屏终端、底部分屏终端、右侧分屏终端、切终端 Buffer、执行命令、发送当前行/选区 |
| `<Space> r t` / `<Space> r l` / `<Space> r o` | `overseer-*` | 选择并运行构建/测试任务、重跑上次任务、开关 `*Overseer*` 任务面板 |
| `<Space> q s` / `<Space> q l` / `<Space> q d` | `session-save` / `restore` / `delete` | 保存当前工作区布局与光标会话、恢复会话、删除已存会话 |
| `<Space> z` / `<Space> z n` | `zen-mode` / `zen-narrow-focus` | 切换居中禅模式 / 对当前选区同时开启 Narrow + Zen 专注编辑 |
| `<Space> u` / `u c` / `u r` / `u i` / `u t` / `u f` / `u l` | UI 与编辑器特性开关 | 打开 `*UndoTree*`、开关真彩色色块、彩虹括号、光标同词高亮、Twilight 聚光灯、保存自动格式化、Lualine 状态栏 |
| `<Space> l` | `lazy` | 打开 `*Lazy*` 单 VM 扩展插件与服务管理面板 |
