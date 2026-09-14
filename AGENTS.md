.rules

## CI 与代码规范
- 任何 Rust 代码修改完成后、提交之前，必须执行格式化与检查：运行 `cargo fmt --all`，确保 `cargo fmt --all -- --check` 能够通过 CI 检查。