# MerkurDB

[![CI](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml/badge.svg)](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92+-orange.svg)](rust-toolchain.toml)

> [English](README.md)

面向 AI Agent 的独立认知记忆服务。灵感源自神经科学，使用 Rust 构建。

单一二进制，零运行时依赖。支持语义搜索、图扩散、记忆巩固和艾宾浩斯遗忘曲线。

> 设计哲学：[SPEC_CN.md](docs/SPEC_CN.md) · 技术架构：[ARCHITECTURE_CN.md](docs/ARCHITECTURE_CN.md)

## 快速开始

```bash
# 启动服务（NoopEmbedder + SQLite）
cargo run --release -p merkur-server -- --config config.example.yaml

# 设置 Bearer token（必须与 config.example.yaml 中 auth.tokens 匹配）
export MERKUR_TOKEN='replace-me-with-a-strong-token'

# 写入记忆
curl -X POST localhost:1934/v1/write \
  -H "Authorization: Bearer $MERKUR_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"content":"v8 GC is generational","context":{"agent":"assistant"}}'

# 搜索
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8+gc&mode=fast'

# 图扩散搜索
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8&mode=deep&depth=2&include_graph=true'

# 健康检查（无需认证）
curl localhost:1934/v1/health
```

## 核心特性

- **双路检索**：S1 快速（向量 top-k）+ S2 深度（SQLite CTE BFS 图扩散）
- **艾宾浩斯遗忘曲线**：指数权重衰减、访问加成、级联降级（Full→Summary→Title→Archive）
- **离线巩固**：LLM 驱动的摘要生成、实体提取和自动边创建
- **插件架构**：Embedder / Storage / Consolidator / Forgetter — 通过 trait + 配置注入独立替换
- **双存储**：SQLite（默认）+ LanceDB 磁盘索引（feature gate）
- **Rust SDK**：`merkur-client` crate，含 `MerkurClient` trait 和 `HttpMerkurClient`
- **OpenAPI 3.0**：多语言 SDK 代码生成

## API

| 方法 | 路径 | 描述 |
|------|------|------|
| `GET` | `/v1/health` | 健康检查 |
| `POST` | `/v1/write` | 写入记忆 |
| `POST` | `/v1/write-batch` | 批量写入 |
| `GET` | `/v1/search` | 搜索（level/category/日期过滤） |
| `GET` | `/v1/memory/{id}` | 获取记忆详情 |
| `PUT` | `/v1/memory/{id}` | 更新（自动重嵌入） |
| `DELETE` | `/v1/memory/{id}` | 删除（级联 edges + tags） |
| `GET` | `/v1/status` | 存储统计 + 运行时间 |
| `POST` | `/v1/consolidate` | 触发巩固 |
| `GET` | `/v1/consolidate/log` | 巩固审计日志 |
| `POST` | `/v1/forget` | 触发遗忘 |
| `POST` | `/v1/relate` | 创建边 |
| `POST` | `/v1/relate-batch` | 批量创建边 |
| `GET` | `/v1/graph/{id}` | 图邻域（含边详情） |

## Docker

```bash
docker build -t merkurdb .
docker run -p 1934:1934 -v ./data:/var/lib/merkur/data merkurdb
```

## MCP 集成

`merkur-mcp` 将 MerkurDB 作为 Model Context Protocol 服务器通过 stdio 暴露。AI 助手（Claude Desktop、Cursor 等）可直接读写记忆。

```bash
# 构建
cargo build --release -p merkur-mcp

# 独立运行（默认使用 NoopEmbedder）
MERKUR_DB_PATH=~/.merkur/data/merkur.db merkur-mcp
```

### Claude Desktop

添加到 `~/Library/Application Support/Claude/claude_desktop_config.json`：

```json
{
  "mcpServers": {
    "merkurdb": {
      "command": "/path/to/merkur-mcp",
      "env": {
        "MERKUR_DB_PATH": "~/.merkur/data/merkur.db"
      }
    }
  }
}
```

### Cursor

添加到项目中的 `.cursor/mcp.json`：

```json
{
  "mcpServers": {
    "merkurdb": {
      "command": "/path/to/merkur-mcp",
      "env": {
        "MERKUR_DB_PATH": "~/.merkur/data/merkur.db"
      }
    }
  }
}
```

### 可用工具

| 工具 | 描述 |
|------|------|
| `write_memory` | 写入新记忆 |
| `search_memory` | 语义相似度搜索 |
| `get_memory` | 按 ID 获取记忆 |
| `delete_memory` | 按 ID 删除记忆 |
| `relate` | 创建记忆间的关联边 |

## 开发

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-features -- -D warnings

# Feature gates
cargo build --features openai,lancedb
```

## 项目结构

```
crates/
├── core/              # 类型 + trait + 错误
├── storage/           # SQLite + LanceDB 后端
├── embedders/         # Noop / Ollama / OpenAI
├── consolidators/     # Noop / LLM
├── forgetters/        # Ebbinghaus
├── server/            # HTTP 服务 + scheduler
└── client/            # Rust SDK
```

## 路线图

### 已完成

#### v0.1.0 — 基础

| 类别 | 特性 |
|------|------|
| Core | 类型系统（Memory, Edge, MemoryLevel），4 个插件 trait，MerkurError |
| Storage | SqliteStorage（WAL + r2d2），InMemoryVectorIndex（余弦相似度） |
| Storage | LanceDbStorage（磁盘向量搜索，feature gate） |
| Embedders | NoopEmbedder，OllamaEmbedder，OpenAIEmbedder（feature gate） |
| Retrieval | S1 快速（向量 top-k），S2 深度（CTE BFS 图扩散） |
| Consolidation | NoopConsolidator，LlmConsolidator（LLM 摘要 + 边创建） |
| Forgetting | EbbinghausForgetter（指数衰减 + 访问加成 + 级联） |
| Server | 14 REST 端点，CORS，Scheduler，优雅关闭 |
| SDK | `merkur-client` crate，OpenAPI 3.0 spec |
| DevOps | Docker，GitHub Actions CI |

#### v0.2.0 — 加固

| 类别 | 特性 |
|------|------|
| Security | Bearer-token 认证中间件，恒定时间比较 |
| Safety | 每连接 `foreign_keys=ON`，所有 SQLite 操作 `spawn_blocking` |
| Correctness | 艾宾浩斯公式修正（真半衰期），BFS 环检测 |
| Performance | 有界最小堆 top-k，批量 `json_each` 查询 |
| Config | Figment 多层合并，运行时校验 |
| API | 结构化错误响应，请求体限制（10 MiB） |

#### v0.3.0 — 性能与可靠性

| 类别 | 特性 |
|------|------|
| 关键修复 | 巩固不再将失败的记忆标记为已完成 |
| Performance | 5 条热路径 N+1 消除（bfs, write_batch, search, graph, relate） |
| Performance | 向量索引预缓存 L2 范数，LanceDB 256 行自动建索引 |
| Security | `subtle` crate 恒定时间 token 比较 |
| API | `write_batch` 全失败返回 207，上下文 boost 先于阈值过滤 |
| Cleanup | 删除死代码（Timeout/Unauthorized variants, rebuild_vector_index） |
| Docs | Mermaid 图（crate 依赖、检索流程、生命周期、巩固时序） |

### 规划中 (v0.4.0+)

| 优先级 | 特性 | 描述 |
|--------|------|------|
| P1 | MCP adapter | Model Context Protocol 集成，Agent 直接访问 |
| P1 | gRPC API | 基于 `tonic` 的高性能流式 API |
| P2 | 静态加密 | SQLCipher 或应用层 embedding 列加密 |
| P2 | DB 迁移工具 | Schema 版本化，`merkur migrate` CLI |
| P2 | PostgreSQL 后端 | 通过 Storage trait 的 PG 存储后端 |
| P2 | Rust CLI | `merkurctl` — 管理操作（触发巩固、查询状态、备份） |
| P3 | Web 仪表盘 | Tauri/Yew SPA — 记忆图可视化、配置编辑器 |
| P3 | 多模态 | 图像嵌入支持（CLIP 等） |
| P3 | 分布式巩固 | 多 Worker 并行巩固 |

## 文档

- [SPEC_CN.md](docs/SPEC_CN.md) — 设计哲学、认知科学背景、产品路线
- [ARCHITECTURE_CN.md](docs/ARCHITECTURE_CN.md) — 技术架构、数据模型、API 规范
- [openapi.yaml](openapi.yaml) — OpenAPI 3.0 规范
- [CHANGELOG.md](CHANGELOG.md) — 变更日志

## 许可证

MIT
