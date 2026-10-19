#!/usr/bin/env bash
# 提交前检查：格式、静态分析、全量测试、UI 布局、后端压测。
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets -- --test-threads=1

# UI 布局用例带特性才编译，单独跑并确认真的执行了而不是 0 条通过
ui_output=$(cargo test -p chat-ui-gpui --features ui-test --test ui_layout 2>&1)
echo "$ui_output"
if grep -q "running 0 tests" <<<"$ui_output"; then
    echo "UI 布局测试没有执行：确认 --features ui-test 是否生效" >&2
    exit 1
fi

cargo test -p chat-server --test load -- --nocapture
