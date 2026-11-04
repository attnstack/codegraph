// postinstall：按平台从 GitHub Release 下载对应的原生二进制。
// 已有二进制（重装/离线）或设置了 CODEGRAPH_BIN 时跳过下载。
// 中国大陆网络可设 CODEGRAPH_GH_PROXY=https://ghproxy.example 加速下载。

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const VERSION = require("./package.json").version;
const BIN_DIR = path.join(__dirname, "bin");

const ARTIFACTS = {
  "win32_x64": "codegraph-windows-x64.exe",
  "linux_x64": "codegraph-linux-x64",
  "darwin_arm64": "codegraph-macos-arm64",
};

function main() {
  fs.mkdirSync(BIN_DIR, { recursive: true });
  const ext = process.platform === "win32" ? ".exe" : "";
  const native = path.join(BIN_DIR, `codegraph${ext}`);

  if (process.env.CODEGRAPH_BIN && fs.existsSync(process.env.CODEGRAPH_BIN)) {
    // 复用用户已有的二进制
    fs.copyFileSync(process.env.CODEGRAPH_BIN, native);
    finish();
    return;
  }
  if (fs.existsSync(native)) {
    finish(); // 重复安装/离线重装
    return;
  }

  const key = `${process.platform}_${process.arch}`;
  const artifact = ARTIFACTS[key];
  if (!artifact) {
    console.warn(
      `[codegraph-mcp] 不支持的平台 ${key}：请从 GitHub Releases 手动下载，或用 cargo install codegraph`,
    );
    process.exit(0); // 不让安装失败——包仍可用于源码构建场景
  }

  const proxy = process.env.CODEGRAPH_GH_PROXY || "";
  const url =
    `${proxy}https://github.com/attnstack/codegraph/releases/download/v${VERSION}/${artifact}`;
  console.log(`[codegraph-mcp] 下载 ${url}`);
  try {
    execFileSync("curl", ["-fsSL", "-o", native, url], { stdio: "inherit" });
  } catch (e) {
    console.warn(`[codegraph-mcp] curl 下载失败（${e.status || e.message}），尝试 Node fetch`);
    // curl 不在的环境用 Node 22+ 内置 fetch
    (async () => {
      const resp = await fetch(url, { redirect: "follow" });
      if (!resp.ok) {
        console.warn(`[codegraph-mcp] 下载失败 ${resp.status}；npm 安装继续（二进制可后补）`);
        process.exit(0);
      }
      fs.writeFileSync(native, Buffer.from(await resp.arrayBuffer()));
      finish();
    })();
    return;
  }
  finish();
}

function finish() {
  if (process.platform !== "win32") {
    try {
      fs.chmodSync(path.join(BIN_DIR, "codegraph"), 0o755);
    } catch {
      /* bin 可能还没下载成功 */
    }
  }
  console.log("[codegraph-mcp] 安装完成。MCP 配置：command = codegraph, args = [\"mcp\", \"--root\", <repo>]");
}

main();
