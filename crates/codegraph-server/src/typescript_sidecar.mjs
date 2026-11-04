// tsc 类型消歧 sidecar（codegraph 二进制内嵌，运行时解压到临时目录调用）。
// 协议：stdin 收一行 JSON 指令，stdout 回一行 JSON 结果。
//   {"cmd":"init","root":"...","tsPath":".../node_modules/typescript"}
//   → {"ok":true,"errors":0}
//   {"cmd":"scan","files":["src/a.ts",...]}
//   → {"ok":true,"calls":[{"file":"src/a.ts","line":10,"prop":"charge",
//        "recvType":"Checkout","recvFile":"src/services/checkout.ts","recvLine":4}, ...]}
//
// 设计要点：一次性 createProgram（全项目类型图，构建最贵），之后逐文件扫描成员调用；
// 接收者类型回传「声明文件 + 声明行 + 类型名」三元组，Rust 侧用 (recvFile, 类型名)
// 命中图内的类节点——比 typeToString 的字符串匹配可靠得多。
// 只用 typescript 包本身，不引任何第三方依赖。

import { createRequire } from "node:module";
import { createInterface } from "node:readline";
import * as fs from "node:fs";
import * as path from "node:path";

const require = createRequire(import.meta.url);
let ts = null;
let checker = null;
let sourceFiles = null;
let program = null;
let errors = 0;

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function init(msg) {
  const tsPath = path.join(msg.tsPath, "lib", "typescript.js");
  if (!fs.existsSync(tsPath)) {
    send({ ok: false, error: `typescript not found at ${tsPath}` });
    process.exit(1);
  }
  ts = require(pathToFileUrl(tsPath));
  const configPath = findTsConfig(msg.root);
  const configFile = configPath
    ? ts.readConfigFile(configPath, ts.sys.readFile)
    : { config: {} };
  const parsed = ts.parseJsonConfigFileContent(
    configFile.config,
    ts.sys,
    path.dirname(configPath || msg.root),
    { noEmit: true },
    configPath,
  );
  program = ts.createProgram({
    rootNames: parsed.fileNames,
    options: { ...parsed.options, noEmit: true, skipLibCheck: true },
    projectReferences: parsed.projectReferences,
  });
  checker = program.getTypeChecker();
  for (const d of program.getSemanticDiagnostics()) {
    errors++;
  }
  send({ ok: true, errors, files: program.getSourceFiles().length });
}

function pathToFileUrl(p) {
  const u = p.replace(/\\/g, "/");
  return "file://" + (u.startsWith("/") ? u : "/" + u);
}

function findTsConfig(root) {
  let dir = path.resolve(root);
  for (let i = 0; i < 10; i++) {
    const candidate = path.join(dir, "tsconfig.json");
    if (fs.existsSync(candidate)) return candidate;
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

// 找到声明节点的 (文件, 行)，相对项目根
function declLocation(symbol) {
  const decl = symbol?.declarations?.[0];
  if (!decl) return null;
  const sf = decl.getSourceFile();
  const { line, character } = sf.getLineAndCharacterOfPosition(decl.getStart());
  const file = path.relative(ROOT, sf.fileName).replace(/\\/g, "/");
  return { file, line: line + 1, character: character + 1 };
}

function scan(msg) {
  const calls = [];
  for (const relFile of msg.files) {
    const abs = path.resolve(ROOT, relFile);
    const sf = sourceFiles.get(abs) || program.getSourceFile(abs);
    if (!sf) continue;
    sourceFiles.set(abs, sf);
    sf.forEachChild(function visit(node) {
      if (ts.isCallExpression(node)) {
        const expr = node.expression;
        if (ts.isPropertyAccessExpression(expr)) {
          const prop = expr.name.text;
          const type = checker.getTypeAtLocation(expr.expression);
          let symbol = type.getSymbol() || type.aliasSymbol;
          if (!symbol && type.isUnion && type.isUnion()) {
            symbol = type.types[0]?.getSymbol();
          }
          const loc = sf.getLineAndCharacterOfPosition(node.getStart());
          const recv = symbol ? declLocation(symbol) : null;
          const recvType = checker.typeToString(type, undefined, ts.TypeFormatFlags.UseFullyQualifiedType);
          calls.push({
            file: relFile,
            line: loc.line + 1,
            prop,
            recvType,
            recvFile: recv ? recv.file : null,
            recvLine: recv ? recv.line : null,
          });
        }
      }
      node.forEachChild(visit);
    });
  }
  send({ ok: true, calls });
}

const rl = createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return send({ ok: false, error: "bad json" });
  }
  try {
    if (msg.cmd === "init") {
      sourceFiles = new Map();
      return init(msg);
    }
    if (msg.cmd === "scan") {
      return scan(msg);
    }
    if (msg.cmd === "ping") {
      return send({ ok: true });
    }
    send({ ok: false, error: `unknown cmd ${msg.cmd}` });
  } catch (e) {
    send({ ok: false, error: String(e?.stack || e) });
  }
});
rl.on("close", () => process.exit(0));
