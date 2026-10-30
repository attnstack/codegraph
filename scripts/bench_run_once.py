#!/usr/bin/env python3
"""高精度 bench 采集（subprocess + perf_counter，多次运行取中位数）"""
import statistics
import subprocess
import time

import os
CG = os.environ.get("CODEGRAPH_BIN", os.path.abspath(os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "codegraph.exe")))
B = os.environ.get("CODEGRAPH_BENCH_DIR", os.path.join(
    os.environ.get("TEMP", "/tmp"), "codegraph-bench-100k"))
N = 7


def run(label, args, n=N, setup=None):
    times = []
    for i in range(n):
        if setup:
            setup()
        t0 = time.perf_counter()
        subprocess.run([CG] + args, capture_output=True)
        times.append((time.perf_counter() - t0) * 1000)
    times.sort()
    print(f"{label:24s} min={times[0]:8.1f}ms  median={statistics.median(times):8.1f}ms")


def touch():
    with open(B + r"\src\file_0.ts", "a", encoding="utf-8") as f:
        f.write(f"// tick {time.time()}\n")


run("index --force (warm)", ["index", "--root", B, "--force"], n=5)
run("update (1 comment line)", ["update", "--root", B], n=5, setup=touch)
run("who-calls", ["who-calls", "fn_0_0", "--root", B])
run("impact", ["impact", "fn_0_0", "--root", B])
run("hotspots", ["hotspots", "--top", "10", "--root", B])
run("trace", ["trace", "fn_0_0", "fn_100_0", "--root", B])
run("search", ["search", "fn_0_0", "--root", B])
t0 = time.perf_counter()
subprocess.run(["grep", "-rn", "fn_0_0", B + r"\src"], capture_output=True)
print(f"{'grep-baseline':24s} {(time.perf_counter()-t0)*1000:8.1f}ms (single run)")
