# 面向 Lava 配置与插件开发的 Nessa 语言手册 *nessa-for-lava*

**Nessa** 是驱动整个 Lava 应用层、40 个内置扩展以及用户 `~/.config/lava/init.ns` 的现代静态/渐进混合强类型编程语言。它将 Rust/ML 级别的数据类型与模式匹配、受检错误联合类型（`!Err T`）、**代数效应（Algebraic Effects）**、**能力处理器（Capabilities）** 以及 **多提示符定界延续（Multi-Prompt Delimited Continuations）** 凝聚在极度清爽、无需分号的语法之中。

本手册专为编写 `init.ns` 与开发 Lava 插件的读者提炼最核心的 Nessa 实战语法与惯用法。返回手册总目录请参阅 `[index#top]`；若需查阅编译器底层完整形式化规范，可在 `*Info*`（`C-h i`）中直接跳转至 `[nessa:grammar/intro#top]`。

---

## 基础语法、绑定、函数与数据类型 *nessa-types*

### 注释、变量绑定与字符串拼接 *nessa-basics*

- **行注释**：以双连字符 `--` 开头直到行尾（注意不是 `//`）。
- **不可变与可变绑定**：`let x = 42` 声明不可变绑定；`var count: i64 = 0` 声明可重新赋值的可变绑定（支持 `+=`、`-=`）。
- **常用基础类型**：`i64`、`u64`、`usize`、`f64`、`bool`、`char`、`String`、`Unit`（单值写作 `()`）、动态顶层类型 `Any`（配合 `x.as(TargetType)` 安全转型与 `x matches _: TargetType` 运行时类型判断）。
- **字符串操作**：使用 `++` 拼接两个字符串（如 `"Hello, " ++ name`），在双引号字面量内也支持 `{expr}` 字符串插值（当需要在字面量中输出字面花括号时写作 `{{` 与 `}}`）。

### 函数定义、具名默认参数与闭包 *nessa-functions*

Nessa 函数支持单表达式简写（`= expr`）、具名可选参数（以点号 `.param: Type = default` 声明）以及竖线闭包 `|arg: Type| -> Ret { ... }`：

```nessa
-- 单表达式函数
pub fn ext_my_id() -> ExtId = ExtId("user.local/ext_my")

-- 带具名默认参数的函数（调用时写作 clamp_pos(p, insert_mode = true)）
pub fn clamp_pos(self, pos: Pos, .insert_mode: bool = false) -> Pos {
    -- 函数最后一个表达式即返回值，也可提前使用 `return expr`
    ...
}

-- 条件分支：`if ... { ... } else { ... }` 与多分支守卫 `when { ... }`
let badge = when {
    score >= 8000 => "PREFIX",
    score >= 4000 => "SUBSTR",
    else => "FUZZY",
}
```

### `struct`、`enum`、`newtype` 与模式匹配 *nessa-structs-enums*

Lava 大量使用名义新类型（`newtype`）消除字符串/整数 ID 混用错误，并用代数数据类型（`enum`）建模编辑器状态：

```nessa
-- 1. 名义新类型与 trait 自动派生（通过 `.raw()` 取出底层值）
pub newtype BufferId = i64
derive Eq, PartialEq, Ord, PartialOrd, Hash, Display for BufferId

-- 2. 结构体与方法实现块 (`impl`)
pub struct Pos {
    pub row: i64,
    pub col: i64,
}

impl Pos {
    pub fn zero() -> Pos = Pos { row: 0, col: 0 }
}

-- 3. 带载荷枚举与后缀 `match` 表达式
pub enum VimMode {
    Normal,
    Insert,
    VisualChar,
    OperatorPending(String),
}

mode match {
    VimMode.Normal => "NORMAL",
    VimMode.OperatorPending(op) => "OP(" ++ op ++ ")",
    _ => "OTHER",
}
```

> 更多类型系统细节请参阅底层规范 `[nessa:type-system/intro#top]`。

---

## 可选类型（`?T`）与受检错误流（`!Err T`） *nessa-optional-and-errors*

Nessa 通过高度对称的限定类型（Qualified Types）彻底告别空指针异常与隐式未捕获异常：

### 可选类型 `?T` 与 `let x? = ... else` 解包 *nessa-optional*

`?T` 表示值既可能是 `T` 也可能是 `null`：

```nessa
let item_opt: ?String = list_or_null()

-- 惯用法 1：`let name? = expr else { 提前返回或回退块 }`
let text? = ask_prompt(req)# else {
    -- 当返回 null（例如用户按了 <Escape> / <C-g>）时进入此分支
    return ()
}
-- 此处 `text` 已自动窄化为非空 `String`！

-- 惯用法 2：在 `match` 中使用 `val? =>` 匹配非空分支
opt_reg match {
    rname? => println("Recorded @" ++ rname),
    _ => (),
}
```

### 受检错误类型 `!Err T` 与 `!` 传播算子 *nessa-errors*

返回类型写作 `!String Buffer` 表示函数成功时返回 `Buffer`，失败时通过 `return error "reason"` 返回 `String` 类型的可恢复错误：

```nessa
pub fn open_config(path: String) -> !String String {
    -- 1. 后缀 `!`：成功则解包值，失败则立即将错误向上层调用者传播（类似 Rust 的 `?`）
    let world = require_editor_world(kernel)!

    -- 2. `let raw! = expr else { ... }`：内联拦截错误并降级处理
    let raw! = fs.read_to_string(path) else {
        return error ("cannot read file: " ++ path)
    }

    -- 3. `expr ! { err => fallback }`：错误消除块
    raw
}
```

---

## 代数效应（`effect`）与能力处理器（`handles`） *nessa-effects*

代数效应是 Lava 实现“核心逻辑零平台耦合、可 100% 纯内存单元测试”的核心秘密武器（详见底层规范 `[nessa:algebraic-effect-and-multi-prompt-delimited-continuation/algebraic-effect#top]`）：

### 声明、发出（`#`）与组合效应集 *effect-declaration-and-call*

在 `packages/lava_core/src/lib.ns` 中，日志、状态栏消息与剪贴板均声明为 `effect`：

```nessa
pub effect log_event(level: String, source: String, message: String) -> Unit
pub effect status_emit(message: String) -> Unit
pub effect clipboard_get() -> String
pub effect clipboard_set(text: String) -> Unit

-- 使用类型别名打包常用效应集合
pub typealias ServiceEffects = [log_event, status_emit, clipboard_get, clipboard_set]
```

**核心语法规则**：
- **与 `!` 完全对称的 `#` 后缀算子**：调用效应或调用带效应的函数时，在末尾加上 `#` 表示发出/向上传播效应（例如 `status_emit("Saved!")#`）；
- **同时传播效应与错误（`#!`）**：当被调函数返回 `#ServiceEffects !String Bool` 时，调用点写作 `let ok = my_cmd(kernel)#!`——先以 `#` 传播效应，再以 `!` 解包或传播错误！

### 用 `handles(...)` 定义有状态 Capability 处理器 *nessa-capabilities*

在 `ext_app`（以及所有测试桩）中，通过为结构体方法标注 `handles(effect_name)` 将实例转化为效应处理器，并用 `handles(eff) let cap = ...` 或 `.use(...)` 注入词法作用域：

```nessa
pub struct StatusCollectorCap {
    pub messages: List,
}

impl StatusCollectorCap {
    pub fn on_status(self, message: String) -> Unit handles(status_emit) {
        self.messages.push(message)
    }
}

fn run_test() {
    handles(status_emit) let cap = StatusCollectorCap { messages: List() }
    -- 此作用域内任何深层函数发出的 `status_emit(...)#` 都会零开销就地调用 `cap.on_status(...)`！
    status_emit("ready")#
}
```

由于上述效应未声明 `catch k`，Nessa 编译器采用 **In-Place Handler Application（就地闭包调用）** 编译策略，无任何栈拷贝或延续分配开销！

---

## 多提示符定界延续（`catch k`）与异步效应 *nessa-continuations*

### 显式捕获定界延续实现非阻塞交互提示 *delimited-continuations*

当一个效应需要在不阻塞主线程的前提下**暂停当前计算栈、返回事件循环等待未来用户输入后再恢复**时，只需在 `effect` 声明中加入 `catch k` 参数（详见 `[nessa:algebraic-effect-and-multi-prompt-delimited-continuation/multi-prompt-delimited-continuation#top]`）：

```nessa
-- `lava_core` 中的交互式提示效应声明
pub effect ask_prompt(req: PromptRequest, catch k) -> ?String
```

在 `SystemKernel.dispatch_command` 中，处理器块接收被捕获的独立定界栈段 `k: Continuation`：

```nessa
let outcome = ((spec.handler)(self, args))# {
    ask_prompt(req, k) => {
        -- 不立即调用 k，而是将 k 存入内核并返回 Suspended！
        -- Nessa VM 仅分离 delimiter 边界的独立栈段链，零调用帧复制！
        self.active_prompt = (req, k)
        PromptTurn.Suspended(req, k)
    }
}
```

稍后当用户在 Minibuffer 按下 `<Return>` 时，`submit_prompt_answer` 调用 `k(answer)`，VM 立即重接该栈段链并将 `answer` 作为当初 `ask_prompt(req)#` 的返回值恢复执行！

### `async effect` 与 `Future` 结构化并发 *nessa-async-effects*

在 Nessa 中没有传染性的 `async fn` 染色问题——并发唯一的入口是 **`async effect`**（调用 `async effect` 直接返回一个 `Future` 句柄，配合 `std.task` 的 `await_all` / `await_any` 即可实现并行分片，如 `ext_tools` 中的并发项目搜索！详见 `[nessa:concurrency-and-async-algebraic-effect/intro#top]`）。

---

## 包导入语法与 `std` 标准库高频模块速查 *nessa-stdlib*

### `use` 模块导入规则 *nessa-imports*

- 导入标准库模块：`use std.fs`、`use std.path`、`use std.json`、`use std.collections.{HashMap, Set}`
- 导入工作区跨包依赖（以 `@` 加短包名开头）：`use @lava_core.{ExtId, SystemKernel}`、`use @ext_document.require_editor_world`

### Lava 插件开发最常用的 `std` 标准库模块 *stdlib-quick-table*

完整 22 个标准库模块总表请参阅 `[nessa:standard-library/overview#top]`，以下为编写 Lava 扩展最常使用的核心 API：

| 标准库模块 | 核心类型与高频函数 |
| --- | --- |
| **预导入 (`prelude`)** | `List`（`List()`、`[a, b]`、`.len()`、`.is_empty()`、`.push(v)`、`.pop()`、`.get(i)`、`.set(i, v)`、`.sort()`、`.sort_by(fn)`、`.join(sep)`）、`String`（`.char_len()`、`.slice(lo, hi)`、`.take(n)`、`.drop(n)`、`.trim()`、`.split(sep)`、`.split_once(sep)`、`.starts_with()`、`.ends_with()`、`.contains()`、`.replace()`、`.to_lower()`、`.to_upper()`、`.parse_i64()`）、`HashMap`（`.new()`、`.insert(k, v)`、`.get(k)`、`.contains_key(k)`、`.remove(k)`、`.keys()`）、`Set`、`Deque`、`println` / `eprintln`、`assert` / `assert_eq` |
| `std.fs` | `read_to_string(p)`、`write_string(p, s)`、`exists(p)`、`is_file(p)`、`is_dir(p)`、`metadata(p)`、`create_dir_all(p)`、`read_dir(p)`、`rename(a, b)`、`copy(a, b)`、`remove(p)` |
| `std.path` | `join(a, b)`、`normalize(p)`、`parent(p)`、`file_name(p)`、`file_stem(p)`、`extension(p)`、`is_absolute(p)` |
| `std.json` | `json.parse(text)`（返回 `!JsonError Any`）与 `json.stringify(val)`（用于持久化会话、Harpoon 书签等状态） |
| `std.pattern` | 完整模式匹配与捕获替换：`find`、`match_once`、`captures`、`gmatch`、`gsub`、`escape` |
| `std.env` & `std.time` | `env.get(name)`、`env.current_dir()`、`time.unix_millis()`、`time.monotonic_nanos()`、`time.format_date(...)` |
| `std.process` & `std.task` | `process.exec(prog, args)`、`process.shell(cmd)`、`task.await_all(futures)`、`task.await_any(futures)` |

掌握以上特性后，您已经具备阅读 Lava 全部 `.ns` 源码并编写任意复杂插件的能力！下一步请结合 `[extension-development#top]` 与 `[plugins-catalog#top]` 动手实践。
