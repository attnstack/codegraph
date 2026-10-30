#!/usr/bin/env bash
# codegraph benchmark 脚本：产出可复现的实测数字。
# 语料：scripts/gen_corpus.py 生成的合成 TS 仓库（固定 seed）。
# 指标：冷启动全量索引 / 单文件变更增量 / 全量重建 / 查询延迟 / grep 基线。
# 用法: bash scripts/bench.sh [files] [funcs]
# 结果人工抄录进 README 的 Benchmark 表格（含机器环境说明）。

set -euo pipefail

FILES="${1:-200}"
FUNCS="${2:-20}"
CORPUS="${CODEGRAPH_BENCH_DIR:-/tmp/codegraph-bench}"
CODEGRAPH="${CODEGRAPH_BIN:-$(pwd)/target/release/codegraph}"

if [[ ! -x "$CODEGRAPH" ]]; then
  CODEGRAPH="$(pwd)/target/debug/codegraph"
fi
if [[ ! -x "$CODEGRAPH" ]]; then
  echo "未找到 codegraph 二进制，先 cargo build --release" >&2
  exit 1
fi

echo "== 环境 =="
echo "binary: $CODEGRAPH"
echo "corpus: $CORPUS (files=$FILES funcs=$FUNCS)"

rm -rf "$CORPUS"
python scripts/gen_corpus.py --out "$CORPUS" --files "$FILES" --funcs "$FUNCS"

bench() {
  # 高精度计时：PowerShell Stopwatch（Git Bash 无 date +%N 保证）
  local label="$1"; shift
  local start end
  start=$(powershell -NoProfile -Command "(Get-Date).Ticks")
  "$@"
  end=$(powershell -NoProfile -Command "(Get-Date).Ticks")
  powershell -NoProfile -Command "('{0} ms' -f (($end - $start) / 10000))"
}

echo "== 1. 冷启动全量索引 =="
bench "cold-index" "$CODEGRAPH" index --root "$CORPUS" --force

echo "== 2. 单文件变更 + 增量更新 =="
echo "// bench touch" >> "$CORPUS/src/file_0.ts"
bench "incremental" "$CODEGRAPH" update --root "$CORPUS"

echo "== 3. 全量重建（同状态） =="
bench "full-rebuild" "$CODEGRAPH" index --root "$CORPUS" --force

echo "== 4. 查询延迟 =="
bench "who-calls" "$CODEGRAPH" who-calls "fn_0_0"
bench "impact" "$CODEGRAPH" impact "fn_0_0"
bench "hotspots" "$CODEGRAPH" hotspots --top 10
bench "trace" "$CODEGRAPH" trace "fn_0_0" "fn_$(($(FILES / 2)))_0"

echo "== 5. grep 基线（who-calls 的朴素替代：文本搜索 fn_0_0 的调用行数） =="
start=$(powershell -NoProfile -Command "(Get-Date).Ticks")
grep -rn "fn_0_0" "$CORPUS/src" >/dev/null
end=$(powershell -NoProfile -Command "(Get-Date).Ticks")
powershell -NoProfile -Command "('{0} ms' -f (($end - $start) / 10000))"

echo "== 完成。请把以上数字连同机器环境（CPU/内存/OS/构建模式）抄进 README Benchmark 表 =="
