// MCP 契约测试：官方 @modelcontextprotocol/sdk 作为标准客户端，
// 通过 StdioClientTransport 驱动 codegraph 二进制，验证协议握手与工具调用。
// 前置：先 `cargo build`（默认找 ../target/debug/codegraph[.exe]，
// 也可用环境变量 CODEGRAPH_BIN 指定其他构建）。

import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import os from "node:os";
import fs from "node:fs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const fixture = path.join(repoRoot, "fixtures", "demo-app");

function findBinary() {
  if (process.env.CODEGRAPH_BIN) return process.env.CODEGRAPH_BIN;
  const exe = process.platform === "win32" ? "codegraph.exe" : "codegraph";
  for (const dir of ["target/debug", "target/release"]) {
    const p = path.join(repoRoot, dir, exe);
    if (existsSync(p)) return p;
  }
  return null;
}

const bin = findBinary();
if (!bin) {
  console.error("未找到 codegraph 二进制：请先 `cargo build`，或设置 CODEGRAPH_BIN");
  process.exit(1);
}

// 每个测试用独立的临时根目录 + 独立索引，互不污染
function makeIndexedRoot() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "codegraph-contract-"));
  // 复制夹具（递归）
  fs.cpSync(fixture, root, { recursive: true });
  execFileSync(bin, ["index", "--root", root], { stdio: "pipe" });
  return root;
}

async function withClient(root, fn) {
  const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
  const { StdioClientTransport } = await import("@modelcontextprotocol/sdk/client/stdio.js");
  const transport = new StdioClientTransport({
    command: bin,
    args: ["mcp", "--root", root],
    stderr: "ignore",
  });
  const client = new Client({ name: "contract-test", version: "0.0.1" });
  await client.connect(transport);
  try {
    await fn(client);
  } finally {
    await client.close();
  }
}

test("initialize 握手与 tools/list 契约", async () => {
  const root = makeIndexedRoot();
  try {
    await withClient(root, async (client) => {
      const tools = await client.listTools();
      const names = tools.tools.map((t) => t.name).sort();
      assert.deepEqual(names, [
        "archaeology",
        "callees",
        "cycles",
        "dead_code",
        "hotspots",
        "impact",
        "search",
        "trace",
        "who_calls",
      ]);
      for (const t of tools.tools) {
        assert.ok(t.description, `工具 ${t.name} 缺少 description`);
        assert.ok(t.inputSchema, `工具 ${t.name} 缺少 inputSchema`);
      }
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("tools/call：search 与 who_calls 返回可解析 JSON", async () => {
  const root = makeIndexedRoot();
  try {
    await withClient(root, async (client) => {
      const search = await client.callTool({ name: "search", arguments: { query: "charge" } });
      assert.ok(!search.isError, "search 不应报错");
      const hits = JSON.parse(search.content[0].text);
      assert.ok(hits.some((n) => n.name === "charge"), `应命中 charge: ${search.content[0].text}`);

      const who = await client.callTool({ name: "who_calls", arguments: { symbol: "charge" } });
      const report = JSON.parse(who.content[0].text);
      const callers = report.callers.map((c) => c.node.qualified_name);
      assert.ok(callers.includes("Checkout.createOrder"), `缺少 createOrder: ${callers}`);
      assert.ok(callers.includes("RefundService.processRefund"), `缺少 processRefund: ${callers}`);
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("tools/call：impact 附证据链；未知符号返回 isError", async () => {
  const root = makeIndexedRoot();
  try {
    await withClient(root, async (client) => {
      const impact = await client.callTool({ name: "impact", arguments: { symbol: "charge" } });
      const report = JSON.parse(impact.content[0].text);
      assert.ok(report.items.length >= 2, "应影响至少 2 个节点");
      for (const item of report.items) {
        const chainTos = item.chain.map((h) => h.to.qualified_name);
        assert.ok(chainTos.includes("charge"), `证据链应回到 charge: ${JSON.stringify(item)}`);
      }

      const bad = await client.callTool({
        name: "who_calls",
        arguments: { symbol: "no_such_symbol_xyz" },
      });
      assert.equal(bad.isError, true, "未知符号应以 isError 结果返回");
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("增量自愈：改动文件后工具结果实时更新", async () => {
  const root = makeIndexedRoot();
  try {
    await withClient(root, async (client) => {
      // 之前：App 不调用 charge
      const before = JSON.parse(
        (await client.callTool({ name: "who_calls", arguments: { symbol: "charge" } })).content[0].text,
      );
      assert.ok(!before.callers.some((c) => c.node.qualified_name === "App"));

      // 改文件：App 直接调用 charge
      const app = path.join(root, "src", "components", "App.tsx");
      fs.writeFileSync(
        app,
        `import { Button } from './Button';\nimport { charge } from '../services/payment';\n\nexport function App(): string {\n  charge(1);\n  return <Button label="hello" />;\n}\n`,
      );

      // auto-refresh 应在下次查询时自动增量重建
      const after = JSON.parse(
        (await client.callTool({ name: "who_calls", arguments: { symbol: "charge" } })).content[0].text,
      );
      assert.ok(
        after.callers.some((c) => c.node.qualified_name === "App"),
        `自动增量更新未生效: ${JSON.stringify(after.callers)}`,
      );
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
