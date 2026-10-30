#!/usr/bin/env python3
"""合成 TS 语料生成器（benchmark 用，纯标准库）。

生成一个层级调用结构的 TypeScript 仓库：
- N 个文件，每文件 F 个导出函数 fn_<file>_<i>
- 文件 i 的函数调用：同文件下一个函数（文件内边）+ 文件 i+1 / i+2 的同名序号函数（跨文件边）
- 固定 seed，语料可复现；调用方向单向（i → i+1），保证无环、链路可预测

用法: python scripts/gen_corpus.py --out bench-corpus --files 200 --funcs 20 [--seed 42]
"""
import argparse
import os
import random
import re


def gen_file(idx: int, total: int, funcs: int, rng: random.Random) -> tuple[str, list[tuple[int, int]]]:
    """返回 (文件内容, 跨文件调用的目标函数列表)"""
    body_lines: list[str] = []
    cross_calls: set[tuple[int, int]] = set()
    for f in range(funcs):
        name = f"fn_{idx}_{f}"
        body = []
        if f + 1 < funcs:
            body.append(f"  fn_{idx}_{f + 1}(n);")
        for t in (idx + 1, idx + 2):
            if t < total and rng.random() < (1.0 if t == idx + 1 else 0.3):
                body.append(f"  fn_{t}_{f}(n);")
                cross_calls.add((t, f))
        body.append("  return n + 1;")
        body_lines.append(f"export function {name}(n: number): number {{")
        body_lines.extend(body)
        body_lines.append("}")
        body_lines.append("")
    header = [f"import {{ fn_{t}_{f} }} from './file_{t}';" for (t, f) in sorted(cross_calls)]
    return "\n".join(header + [""] + body_lines), sorted(cross_calls)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--files", type=int, default=200)
    ap.add_argument("--funcs", type=int, default=20)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    os.makedirs(os.path.join(args.out, "src"), exist_ok=True)
    total_loc = 0
    for i in range(args.files):
        content, _ = gen_file(i, args.files, args.funcs, rng)
        total_loc += content.count("\n")
        with open(os.path.join(args.out, "src", f"file_{i}.ts"), "w", encoding="utf-8") as fh:
            fh.write(content)
    with open(os.path.join(args.out, "package.json"), "w", encoding="utf-8") as fh:
        fh.write('{\n  "name": "bench-corpus",\n  "version": "0.0.0",\n  "private": true\n}\n')
    print(f"生成 {args.files} 个文件，约 {total_loc} 行 -> {args.out}/src")


if __name__ == "__main__":
    main()
