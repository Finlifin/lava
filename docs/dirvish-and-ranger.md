# Dirvish 三栏文件管理器与 Ranger 操作手册 *dirvish*

Lava 内置的 **`ext_dirvish`**（发布服务 `"lava.dirvish.state"`）将 Emacs **Dirvish** 的非阻塞窗口内嵌体验与终端神级文件管理器 **Ranger** 的三栏 **Miller Columns（米勒列）** 空间直觉及全套高频单键/双键操作合二为一。您无需离开当前编辑会话或切换外部终端，按下一个 `-` 即可在毫秒级浏览目录层级、实时预览高亮源码、批量标记复制移动文件，甚至按 `S` 直接在目标目录打开 POSIX PTY 终端。返回手册总目录请参阅 `[index#top]`。

---

## 三栏 Miller Columns 空间布局与智能预览 *miller-columns*

进入 Dirvish 默认展开黄金比例 **20% : 38% : 42%** 的水平三联分屏布局（按 `zp` 或 `<Tab>` 可随时在三栏 Miller 视图与极简宽幅单栏视图之间无损切换）：

```text
┌───────────────────┬────────────────────────────────────┬────────────────────────────────────────┐
│ *dirvish-parent*  │            *dirvish*               │           *dirvish-preview*            │
│    (父目录 20%)    │         (当前活动目录 38%)          │      (实时语法/子目录预览 42%)          │
├───────────────────┼────────────────────────────────────┼────────────────────────────────────────┤
│   crates/         │   [M] ▸ ext_app/                   │ -- ext_dirvish: Emacs Dirvish 3-pane   │
│ ▸ extensions/     │ *     ▸ ext_dirvish/               │ -- Miller-columns file manager...      │
│   packages/       │       ▸ ext_document/              │ use std.fs                             │
│   docs/           │       ▸ ext_help/                  │ use @lava_core.{ExtId, SystemKernel}   │
│   Cargo.toml      │         README.md        4.2 KiB   │ ...                                    │
└───────────────────┴────────────────────────────────────┴────────────────────────────────────────┘
```

1. **左栏（`*dirvish-parent*`，20% 宽）**：展示当前目录的父目录兄弟条目，并高亮当前所在目录的位置。按 `h` 或 `-` 向上回退时，光标会自动聚焦回刚才退出的子目录项，绝不迷失上下文！
2. **中栏（`*dirvish*`，38% 宽，主焦点）**：展示当前目录全部子目录与文件（默认文件夹置顶排序）：
   - **Git 状态实时徽章**：调用 `@lava_accel.accel_git_status` 在条目旁直接渲染仓库状态标记（`M` 已修改、`A` 新增暂存、`?` 未跟踪、`D` 删除）；
   - **多选星号标记**：被 `<Space>` 标记的条目显示醒目的 `*` 前缀；
   - **文件体积与类型图标**：右侧对齐显示人类可读大小（`B` / `KiB` / `MiB` / `GiB`）与目录斜杠标识。
3. **右栏（`*dirvish-preview*`，42% 宽）**：随 `j` / `k` 移动零延迟同步刷新：
   - **文本文件实时语法高亮预览**：自动截取前 `8 KiB` / `120` 行文本并根据文件扩展名推断 `major_mode`（如 `nessa`、`rust`、`toml`、`markdown`、`python`），由 `ext_render` 呈现完整的主题语法着色！
   - **二进制文件安全守卫**：自动探测 NUL（`\0`）字节并展示文件元数据摘要卡片，绝不会因误触大型编译产物或图片而乱码卡死；
   - **目录子项预览**：当光标停留在文件夹上时，右栏直接预览该子目录内部的下级文件树列表。

---

## 打开与退出 Dirvish 的全部入口 *dirvish-open-close*

| 触发方式 | 绑定命令 | 行为说明 |
| --- | --- | --- |
| **`-`**（Normal 模式） | `dirvish-jump` | **最常用入口**（源自 `vim-vinegar`）：打开当前缓冲区文件所在的父目录，并自动将光标精准停在当前文件名所在行！ |
| `<Space> f d` / `<Space> R` | `dirvish` / `ranger` | 在当前工作区根目录（或上次位置）打开三栏 Dirvish |
| `<C-x> d` | `dirvish` | Emacs 经典目录入口直接接入 Dirvish |
| `:Dirvish [dir]` / `:Ranger [dir]` | `ex:Dirvish` / `ex:Ranger` | 打开指定路径（支持 `:Ex` 第二阶段路径补全与 `~/` 展开） |
| `:Ex` / `:Explore` | Ex 内建路由 | 若 `ext_dirvish` 处于激活态则优先打开 Dirvish（否则回退至 Oil / Dired） |
| **`q`**（Dirvish 内） | `dirvish-quit` | 关闭 Dirvish 并自动回收左右辅助预览分屏，干净恢复进入前的源代码窗口布局 |

> **与 Oil 和 Neo-tree 的互补关系**：如果您希望“像编辑普通文本行一样直接修改多行文件名后按 `:w` 批量落盘”，可使用 `<Space> f o` 打开 `ext_oil`；如果您需要在编辑代码的同时常驻左侧树状目录栏，可按 `<Space> n t` 开启 `ext_neotree`（详见 `[plugins-catalog#top]`）。

---

## Ranger 快捷键全表（`mode:dirvish`） *ranger-keys*

在 `*dirvish*` 主缓冲区中，以下全部经典 Ranger 单键与双键序列均原生生效：

### 目录层级与书签跳转 *ranger-navigation*

| 按键 | 内部命令 | 功能说明 |
| --- | --- | --- |
| `j` / `<Down>` | `dirvish-next-line` | 光标下移一项并同步刷新右侧预览栏 |
| `k` / `<Up>` | `dirvish-prev-line` | 光标上移一项并同步刷新右侧预览栏 |
| `h` / `-` / `<Left>` | `dirvish-up-dir` | 退回父目录，并自动选中刚才所在的子目录条目 |
| `l` / `<Right>` / `<Return>` | `dirvish-enter` | 若光标下是目录则深入该子目录；若是文件则收起辅助三栏并在主窗口打开该文件编辑 |
| `gg` / `G` | `dirvish-goto-top` / `bottom` | 跳至当前目录首项 / 末项 |
| `H` / `L` | `dirvish-history-back` / `forward` | 在 Dirvish 目录访问历史栈中后退 / 前进 |
| `gh` | `dirvish-go-home` | 瞬移至用户主目录 `$HOME` |
| `gr` | `dirvish-go-root` | 瞬移至文件系统根目录 `/` |
| `gp` | `dirvish-go-project` | 瞬移回当前 Lava 工作区项目根目录（`world.cwd`） |
| `gn` | `dirvish-new-tab` | 新建一个标签页工作区（Tab）并在其中打开当前目录 |
| `gR` / `r` | `dirvish-refresh` | 强制重新扫描当前磁盘目录与 Git 状态并刷新三栏 |

### 多选标记操作（Marks） *dirvish-marks*

当目录中存在至少一个 `*` 标记项时，后续所有复制（`yy`）、剪切（`dd`）与删除（`dD`）均**批量作用于全部已标记条目**；若无任何标记项，则作用于当前光标所在单一条目：

| 按键 | 内部命令 | 功能说明 |
| --- | --- | --- |
| `<Space>` / `mm` / `t` | `dirvish-toggle-mark` | 切换当前条目的 `*` 选中标记，并自动将光标下移一行（连按 `<Space>` 即可极速连选多行！） |
| `v` | `dirvish-invert-marks` | 反选当前目录下所有可见条目的标记状态 |
| `uv` / `U` | `dirvish-unmark-all` | 一键清空当前目录所有选中标记 |

### 文件复制、剪切、粘贴与安全删除（File Ring） *dirvish-file-ops*

Dirvish 维护独立的跨目录 **File Ring（文件传输环）**，支持在任意目录标记复制/剪切后穿梭到另一目录一键粘贴：

| 按键 | 内部命令 | 功能说明 |
| --- | --- | --- |
| `yy` | `dirvish-yank-copy` | 将已标记项（或当前项）以 **复制（Copy）** 模式装入文件传输环 |
| `ya` | `dirvish-yank-copy-append` | 将当前项**追加**到现有的复制传输环中（支持跨多个源目录分批收集文件！） |
| `dd` | `dirvish-yank-cut` | 将已标记项（或当前项）以 **剪切移动（Move/Cut）** 模式装入文件传输环 |
| `da` | `dirvish-yank-cut-append` | 将当前项**追加**到现有的剪切传输环中 |
| `uy` | `dirvish-clear-ring` | 清空文件传输环，取消待执行的复制/剪切队列 |
| `pp` | `dirvish-paste` | 将传输环中的文件/目录递归复制或移动到当前目录；**若遇到同名目标自动追加 `_copy` 安全去重，绝不静默覆盖已有数据！** |
| `po` | `dirvish-paste-overwrite` | 强制覆盖粘贴到当前目录（显式替换同名目标） |
| `dD` / `D` | `dirvish-delete` | 弹出 `PromptKind.Confirm`（`y/n`）安全确认提示，确认后递归删除已标记（或当前）文件/目录，并同步清理关联的已打开 Buffer |

### 四种精细重命名与新建文件/目录 *dirvish-rename-create*

Ranger 用户最钟爱的“四种重命名初始光标策略”在 Dirvish 中得到了原汁原味的保留：

| 按键 | 内部命令 | 功能说明 |
| --- | --- | --- |
| `cw` / `R` | `dirvish-rename` | 从空白提示符重新输入新文件名（适合彻底改名） |
| `A` | `dirvish-rename-append` | 预填原完整文件名且光标置于末尾（适合在文件末尾追加后缀） |
| `I` | `dirvish-rename-prepend` | 预填原完整文件名且光标置于最前（适合添加前缀） |
| `a` | `dirvish-rename-stem` | 修改主文件名但**自动保留原扩展名**（例如对 `composer.rs` 按 `a` 输入 `builder`，自动重命名为 `builder.rs`！） |
| `cf` / `%` | `dirvish-create-file` | 新建空白文件（若输入路径以 `/` 结尾如 `sub/dir/`，自动递归创建多级目录；否则自动创建父目录并创建文件！） |
| `mk` / `+` | `dirvish-mkdir` | 在当前目录下新建子目录（支持多级嵌套路径 `a/b/c`） |

### 视图切换、实时过滤、排序与路径/终端联动 *dirvish-view-sort-shell*

| 按键 | 内部命令 | 功能说明 |
| --- | --- | --- |
| `zh` / `.` | `dirvish-toggle-hidden` | 显示 / 隐藏以 `.` 开头的隐藏文件与目录（Dotfiles） |
| `zp` / `<Tab>` | `dirvish-toggle-miller` | 在 **三栏 Miller Columns** 与 **单栏全宽模式** 之间一键切换 |
| `zf` / `/` | `dirvish-filter` | 输入关键词实时过滤当前目录条目 |
| `zi` | `dirvish-clear-filter` | 清除当前激活的目录过滤词，恢复显示全部条目 |
| `on` / `os` / `oe` / `or` | `dirvish-sort-*` | 按 **名称（Name）** / **体积（Size）** / **扩展名（Extension）** 排序，或按 `or` **反转升序/降序**（文件夹始终保持分组优先） |
| `yp` / `yd` / `yn` | `dirvish-yank-path` / `dir` / `name` | 将当前条目的 **完整绝对路径** / **所在父目录路径** / **纯文件名** 复制进无名寄存器与系统剪贴板 |
| **`S`** | `dirvish-open-shell` | 收起辅助栏并调用 `terminal-here`，直接在**当前浏览的目录下**打开一个真实 POSIX PTY 交互式终端（详见 `[terminal-and-pty#top]`）！ |
