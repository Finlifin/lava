# Lava 项目协作与测试规范

本文件适用于 `/home/fin/app/lava` 仓库。开发与交付以语义正确、结构清晰和可验证为首要要求。

## 构建与命令入口

- 构建、测试、lint 和清理统一通过 `bash scripts/cargo.sh <cargo 参数>` 执行。该脚本持有 `target.lava-cargo.lock` 独占锁，默认开启 `CARGO_NET_OFFLINE=true`，并在缓存达到 `LAVA_BUILD_CACHE_LIMIT_KIB`（默认 6 GiB）时自动执行 `cargo clean`。
- `Cargo.toml` 依赖 `/home/fin/nessa/nessa2/engine/*`；如 `nessa2` 新增了外部 crate 依赖，需先用 `CARGO_NET_OFFLINE=false bash scripts/cargo.sh fetch` 更新缓存后再离线构建。

## 并行执行测试（默认要求）

运行全量或批量测试时，**默认采用并行执行**，避免逐个串行等待：

1. **Rust 单元测试与集成测试**：
   - 使用并发测试线程：`bash scripts/cargo.sh test --workspace -- --test-threads=8`（或根据 CPU 核数调整）。
   - 多测试二进制可先通过 `bash scripts/cargo.sh test --workspace --no-run` 一次性编译，再并发执行各测试二进制。

2. **`tests_ns/*` Nessa 集成测试包**：
   - 先通过 `bash scripts/cargo.sh build -p lava` 一次性编译 `./target/debug/lava`。
   - 再并发执行各测试包，例如：
     ```sh
     ls -d tests_ns/* | xargs -P 8 -I{} ./target/debug/lava run-package {}
     ```
   - 并行运行需保证各测试使用独立的临时目录与环境变量（如 `LAVA_WINDOW_STATE_FILE`），不共享固定临时文件路径；对实时性、PTY 或显示后端（如 Broadway）敏感的用例若受并发干扰，可在并发批次结束后单独复跑确认。

## 交付前常用检查

```sh
bash scripts/cargo.sh fmt --all -- --check
bash scripts/cargo.sh check --workspace --all-targets
./target/debug/lava check
bash scripts/cargo.sh test --workspace -- --test-threads=8
ls -d tests_ns/* | xargs -P 8 -I{} ./target/debug/lava run-package {}
git diff --check
```
