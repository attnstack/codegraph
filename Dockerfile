# 多阶段构建：编译期带完整 Rust 工具链，运行期只留一个静态链接的二进制。
# 用途：Glama 等目录站通过 Dockerfile 启动 server 做 introspection；本地也可直接用。
#
#   docker build -t codegraph .
#   docker run -i --rm -v /path/to/repo:/repo codegraph        # MCP stdio，默认 --root /repo
#   docker run --rm -v /path/to/repo:/repo codegraph index --root /repo

# builder 与 runtime 必须同一 Debian 代号，否则二进制链接的 glibc 比运行层新会起不来
FROM rust:1.99-slim-bookworm AS builder
WORKDIR /src
# tree-sitter / SQLite(bundled) 需要 C 编译器
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
# 先拷 manifest 预热依赖层，源码变动不重编依赖
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY languages ./languages
RUN cargo build --release --locked -p codegraph-server

FROM debian:bookworm-slim
# archaeology 子命令依赖 git；ca-certificates 供 git 走 https
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/codegraph /usr/local/bin/codegraph
# 挂载点：把待分析仓库挂到 /repo
VOLUME ["/repo"]
WORKDIR /repo
ENTRYPOINT ["codegraph"]
# 无参数启动即 MCP stdio 服务；传子命令则作为 CLI 使用
CMD ["mcp", "--root", "/repo"]
