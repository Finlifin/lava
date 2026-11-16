# Minibuffer 深度指南：Vertico、Orderless、Marginalia 与 Consult *minibuffer*

Lava 的底部交互中枢（`ext_minibuffer`，发布服务 `"lava.minibuffer.controller"`）并非简单的单行输入框，而是集成了现代 Emacs 社区最推崇的四大正交黄金组件设计——**Vertico** 纵向候选视窗、**Orderless** 多分词乱序与取反模糊匹配、**Marginalia** 右对齐元数据注解、**Consult** 实时非侵入预览，外加专为 Evil 用户打造的 **两阶段上下文感知 `:Ex` 补全引擎**。

由于底层完全建立在 `lava_core::ask_prompt` 的多提示符定界延续（Delimited Continuation）之上，无论内置命令还是您在 `init.ns` 中编写的自定义命令，只要调用一次 `ask_prompt`，即自动获得本手册所述的全部交互能力（详见 `[architecture#top]` 与 `[Info: *continuation-prompt-engine*]`）。返回手册总目录请参阅 `[index#top]`。

---

## Vertico：纵向可滚动候选视窗与目录段跳跃 *vertico*

每当激活任何带候选集的提示符（如 `M-x`、`C-x C-f`、`C-x b`、`:`、`/`、`C-h f`、`C-h v`）时，`MinibufferController` 会在底部状态栏上方展开 **Vertico 风格纵向列表**：

- **实时命中统计**：提示符标题栏实时显示匹配进度 `(当前序号/匹配总数)`（例如 `M-x (1/42)`）。
- **循环上下移动**：按 `<C-n>` / `<Down>` 或 `<C-p>` / `<Up>`（以及 `<S-Tab>`）在候选列表中首尾循环移动，视口始终跟随选中项平滑居中滚动。
- **智能 `<Tab>` 补全**：
  - 当仅剩唯一匹配项时，按 `<Tab>` 直接将其填入输入框；
  - 当多项共享更长的公共前缀时，优先补全最长公共前缀（LCP）；
  - 在文件路径提示符中，选中目录按 `<Tab>` 直接深入子目录刷新文件列表！
- **目录层级一键回退（`vertico-directory-up`）**：在 `C-x C-f`（`<Space>ff`）或 `:e path/to/dir/` 等路径上下文中，按 **`<C-l>`** 或 **`<M-BackSpace>`** 无需逐字退格，直接一次性弹出整段父目录（如从 `crates/lava-vm/src/` 瞬间退至 `crates/lava-vm/`）！
- **单键确认模式（`PromptKind.Confirm`）**：对于 `y/n` 或 `query-replace`（`y`/`n`/`!`/`q`）等确认型提示符，无需按 `<Return>`，敲击对应单键立即提交，按其他无效键自动回显合法按键提示。

---

## Orderless：空格多分词乱序匹配与修饰符语法 *orderless* *minibuffer-orderless*

告别必须死记命令前缀或完整路径层级的刻板补全！`ext_minibuffer` 内置完整的 **Orderless 多分词评分器**——您在输入框中以空格分隔输入的多个词元（Tokens）可以按**任意先后顺序**同时约束候选目标，且每个词元均支持精细边界修饰符：

### 五种 Orderless 词元语法 *orderless-syntax*

| 词元写法 | 匹配规则 | 实战查询示例 |
| --- | --- | --- |
| `tok`（普通词元） | **五级加权模糊匹配**：完全相等（10000 分） > 词首前缀（8000 分） > 连字符/斜杠/下划线词边界首字母缩写（6000 分） > 连续子串（4000 分） > 顺序字符子序列（2000 分），并在候选文本中精确高亮每一个命中的字符位置！ | 在 `M-x` 中输入 `tog term` 或 `ttg` 均可命中 `toggleterm-toggle` |
| `!tok`（感叹号排除） | **负向过滤（Negation）**：剔除所有包含子串 `tok` 的候选项（纯负向查询如 `!test` 也会保留所有不含 `test` 的全集项！） | 在文件打开框输入 `lava !test !.md`：查找所有含 `lava` 但排除测试与 Markdown 的源码文件 |
| `^tok`（脱字符前缀） | **锚定前缀匹配**：要求候选字符串最开头（或路径分隔符 `/` 之后的文件名开头）紧跟 `tok` | 在 `C-x C-f` 中输入 `ext ^lib`：仅匹配 `extensions/` 下以 `lib` 开头的文件（如 `lib.ns`） |
| `tok$`（美元符后缀） | **锚定后缀匹配**：要求候选字符串必须以 `tok` 结尾 | 输入 `crates .rs$`：瞬间筛出 `crates/` 下所有 Rust 源码文件 |
| `=tok`（等号字面量） | **精确连续子串匹配**：禁用跳跃子序列匹配，强制要求字面连续出现 `tok` | 输入 `=init.ns !examples`：精确匹配 `init.ns` 并排除 `examples/` 目录 |

> **智能大小写（SmartCase）联动**：Orderless 自动遵循 `world.options.ignorecase` 与 `smartcase`——全小写查询词大小写不敏感；一旦某个词元中含有大写字母（如 `Dirvish` 或 `TSJ`），自动切换为大小写精确匹配。

---

## Marginalia：右对齐富信息徽章与快捷键反查 *marginalia*

在 Vertico 列表的右侧对齐栏，**Marginalia 注解引擎**为每一类候选项实时注入高信息密度的上下文摘要，帮助您在重名或相似项之间一眼做出判断：

- **命令面板（`M-x` / `:Ex`）**：显示 `[cmd]` 或 `[ex]` 类型徽章、**自动反查所有活跃键位表得出的当前快捷键提示**（如 `SPC f f`、`C-x C-f`、`C-h i`）、所属扩展包名，以及命令文档摘要。
- **缓冲区切换（`C-x b` / `:b `）**：显示 `[buf]` 徽章、修改/只读状态标记（`+` / `-`）、主模式（如 `nessa`、`rust`、`markdown`）、总行数与相对工作区路径；最近访问过的历史缓冲区自动获得 MRU 置顶加权与 `[recent]` 标记。
- **文件路径导航（`C-x C-f` / `:e `）**：显示 `[dir]` 或扩展名徽章（`[ns]`、`[rs]`、`[toml]`、`[md]`）、UNIX 权限位（如 `drwxr-xr-x` / `-rw-r--r--`）以及人类可读文件体积（`B` / `KiB` / `MiB`）。
- **选项与主题（`:set ` / `:colorscheme `）**：在 `:set` 补全中直接实时显示该选项在当前会话中的**真实生效值**（如 `[opt=true]`、`[opt=4]`）；在主题补全中标注 `[thm]` 与当前激活主题。

---

## Consult：零副作用实时预览与 `<C-g>` 事务回滚 *consult*

当您在候选列表中按 `<C-n>` / `<C-p>` 浏览尚未按下 `<Return>` 时，**Consult 预览层**已在主编辑区为您同步呈现目标效果，并在会话启动前快照记录 `orig_active_buf_id`、`orig_theme_name` 与光标原点：

1. **实时缓冲区预览**：执行 `C-x b`、`<Space> b b` 或输入 `:b ` 后上下移动候选 Buffer，主窗格立即切换展示对应缓冲区内容与语法高亮；一旦按 `<Escape>` 或 `<C-g>` 取消，焦点瞬间恢复至起跳前的原缓冲区，不污染访问历史。
2. **实时全窗配色预览**：输入 `:colorscheme `（或 `:theme ` / `:colo `）后上下浏览 8 套主题，整个编辑器的背景、状态栏、分屏线与代码高亮实时换肤；按 `<Escape>` / `<C-g>` 立即恢复原配色。
3. **实时增量搜索与全文命中列表（`/`、`?`、`C-s`、`C-r`）**：输入搜索词的每一个字符时，主窗格光标实时跳向最近匹配处并高亮所有命中，同时 Minibuffer 纵向列表列出当前缓冲区内所有匹配的 `[hit] 行号:列号 代码摘要`！在搜索提示符内继续按 `<C-s>` / `<C-r>` 可直接前后巡航，按 `<C-g>` 则将光标与视口精确弹回搜索前的原始行列。

---

## 两阶段上下文感知 `:Ex` 补全引擎 *ex-completion*

按下 `:` 打开 Evil Ex 命令行时，`ext_minibuffer` 会解析可选的行范围前缀（如 `%`、`1,20`、`'<,'>`）并自动切换两阶段补全状态机：

### 第一阶段（Stage 1）：范围感知动词与全局命令补全 *ex-stage1*

当尚未输入参数空格时（例如输入 `:`、`:col` 或 `:'<,'>s`）：
- 同时补全内置 Ex 动词（`e`、`w`、`wq`、`bd`、`sp`、`vsp`、`set`、`colorscheme`、`s/`、`g/` 等）、所有插件注册的 `ex:<Name>` 命令（如 `Dirvish`、`ToggleTerm`、`Lazy`、`CheckHealth`、`ZenMode`、`TSJToggle`）以及 `SystemKernel` 中的交互命令；
- 保留用户已输入的范围前缀（如在 `:'<,'>` 后输入 `nar` 按 `<Tab>` 补全为 `:'<,'>narrow-to-region`）；
- 对需要参数的动词按 `<Tab>` 会自动补上动词与尾随空格，无缝滑入第二阶段！

### 第二阶段（Stage 2）：按动词分派的专业参数补全 *ex-stage2*

一旦动词后出现空格，`ext_minibuffer` 根据动词语义自动挂载对应的专用数据源与 Orderless 过滤器：

| `:Ex` 动词族 | 第二阶段自动挂载的补全数据源与特性 |
| --- | --- |
| `:e `, `:edit `, `:w `, `:sp `, `:vsp `, `:tabnew `, `:cd `, `:Dirvish ` | 工作区与磁盘实时目录/文件路径补全（支持 `~/` 展开与 `<C-l>` 回退上级目录） |
| `:b `, `:buffer `, `:bd `, `:bdelete ` | 已打开的 `Buffer` 名称与编号补全（带 Consult 实时缓冲区预览！） |
| `:set ` | 全部 `:set` 选项名与 `no*` 反置项补全（**支持空格多选项连续补全**，每输入完一项加空格即可继续补全下一项，右侧实时回显当前选项值！） |
| `:colorscheme `, `:colo `, `:theme ` | 8 套内置主题名称补全（上下移动候选时实时全窗换肤预览！） |
| `:Telescope ` | Telescope 子选择器补全（`files`、`live_grep`、`buffers`、`symbols`、`keymaps`、`colorscheme`、`help_tags`、`docs`、`resume`） |
| `:h `, `:help `, `:Info ` | 所有已索引文档手册的 `*help-tag*` 锚点、`manual#slug` 节点与全部命令/选项名称补全 |
| `:Term `, `:terminal `, `:vterm `, `:ToggleTerm ` | 系统可用 Shell 路径与 `1..9` 终端会话编号提示 |

> **关键字面量保护规则（Literal Submission）**：在 `:` 命令行或路径框中，只要您没有主动按 `<C-n>`/`<C-p>`/`<Tab>` 选中下方候选条目（`selection_touched == false`），直接敲击 `<Return>` **100% 原样提交您手敲的字符串**！这保证了输入 `:w new_file.ns`、`:%s/foo/bar/g` 或 `:42` 时绝不会被模糊匹配误替换成列表中的近似项。

---

## Minibuffer 编辑键位与分类历史记录 *minibuffer-keys-and-history*

`MinibufferController` 为不同 `PromptKind`（命令、文件路径、缓冲区、Ex 命令、搜索、普通文本）分别维护独立的去重历史栈（每类上限 100 条），并在通过 `<M-p>` 回翻历史时自动暂存您写到一半的草稿（`saved_draft`）：

| 按键 | 功能说明 |
| --- | --- |
| `<Return>` / `<C-j>` / `<C-m>` | 确认提交当前选中项（或未触碰列表时的字面输入），恢复定界延续计算 |
| `<Escape>` / `<C-g>` | 取消当前提示符，自动回滚所有 Consult 实时缓冲区/配色/搜索预览 |
| `<C-n>` / `<Down>` | 向下选择下一个候选项并刷新实时预览 |
| `<C-p>` / `<Up>` / `<S-Tab>` | 向上选择上一个候选项并刷新实时预览 |
| `<Tab>` | 智能补全公共前缀、深入子目录或推进 `:Ex` 下一阶段 |
| `<C-l>` / `<M-BackSpace>` | 在路径上下文中一键回退至父目录；非路径上下文中 `<M-BackSpace>` 向前删词入 `KillRing` |
| `<M-p>` / `<M-n>` | 向上回溯更早的历史输入 / 向下回到更新的历史输入乃至最初暂存的草稿 |
| `<C-a>` (`<Home>`) / `<C-e>` (`<End>`) | 移动输入光标至行首 / 行尾 |
| `<C-b>` (`<Left>`) / `<C-f>` (`<Right>`) | 左移 / 右移输入光标一列 |
| `<BackSpace>` (`<C-h>`) / `<Delete>` (`<C-d>`) | 删除光标前 / 光标处字符 |
| `<C-w>` / `<C-k>` / `<C-u>` | 向前删词入 `KillRing` / 剪切至行尾入 `KillRing` / 清空光标前整段输入 |
| `<C-y>` | 将 `KillRing` 最新内容粘贴至当前输入框光标处（换行自动转为空格） |
| `<C-s>` / `<C-r>`（搜索提示符内） | 在当前缓冲区内直接跳向下一处 / 上一处匹配（空输入时自动召回上次搜索词） |
