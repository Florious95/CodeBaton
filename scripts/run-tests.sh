#!/usr/bin/env bash
# CodeBaton 测试 runner：跑测试，**跑完自动清理编译产物**，防止 target/ 膨胀到 10GB+。
#
# 用法：
#   scripts/run-tests.sh                      # 跑全量 workspace 测试，跑完 clean
#   scripts/run-tests.sh -p codebaton-app --test sync_safety   # 透传给 cargo test
#   KEEP_TARGET=1 scripts/run-tests.sh        # 跑完**不** clean（迭代开发时保留增量缓存）
#   CLEAN_ONLY=1 scripts/run-tests.sh         # 只清理不跑测试
#
# 设计：
# - 先跑测试、原样保留并打印测试退出码（pass/fail 用户看得到），**再**清理。
# - 默认 `cargo clean` 清空 target/（最可靠的不膨胀保证）。迭代开发设 KEEP_TARGET=1 保留。
# - 顺带清理框架可能残留的临时数据：失败 evidence 目录、共享空 codex 目录。
#   （正常用例的 RUN_ROOT TempDir 已 RAII 自动回收，这里只兜底 abort/SIGKILL 残留。）

set -uo pipefail
cd "$(dirname "$0")/.."   # 切到工程根

cleanup_artifacts() {
  echo ""
  echo "==> 清理编译产物与临时残留（防止 target/ 膨胀）..."
  # 1. 编译产物：target/ 是主要膨胀源（可达 20GB+）。
  cargo clean 2>/dev/null && echo "    cargo clean ✓ (target/ 已清空)" || echo "    cargo clean 跳过（无 target/）"
  # 2. 框架临时残留（RAII 已回收正常用例，这里兜底 hard-abort/SIGKILL 漏网的）。
  local tmp="${TMPDIR:-/tmp}"
  rm -rf "$tmp"/aisync-test-empty-codex "$tmp"/aisync-evidence-* 2>/dev/null
  rm -rf "$tmp"/.tmp* 2>/dev/null || true   # tempfile 默认前缀；仅清本工具留下的，失败忽略
  echo "    临时残留清理 ✓"
}

if [[ "${CLEAN_ONLY:-0}" == "1" ]]; then
  cleanup_artifacts
  exit 0
fi

echo "==> 运行测试：cargo test $*"
cargo test "$@"
rc=$?

if [[ $rc -eq 0 ]]; then
  echo ""
  echo "==> ✅ 测试全部通过"
else
  echo ""
  echo "==> ❌ 测试失败（退出码 $rc）"
fi

# 无论通过与否都清理（除非 KEEP_TARGET=1）。失败时也清——避免反复跑攒一堆 target/。
if [[ "${KEEP_TARGET:-0}" == "1" ]]; then
  echo "==> KEEP_TARGET=1，保留 target/（下次增量编译更快）"
else
  cleanup_artifacts
fi

exit $rc
