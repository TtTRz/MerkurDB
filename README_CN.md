# MerkurDB

> [English](README.md)

外挂式认知记忆服务 — 为 AI Agent 提供长时记忆。受神经科学启发，Rust 全栈实现。

单个二进制，零运行时依赖。支持语义检索、图扩散、记忆巩固和 Ebbinghaus 遗忘曲线。

> 设计理念: [SPEC_CN.md](docs/SPEC_CN.md) · 技术架构: [ARCHITECTURE_CN.md](docs/ARCHITECTURE_CN.md)

## 快速开始

```bash
# 启动服务 (NoopEmbedder + SQLite)
cargo run --release -p merkur-server -- --config config.example.yaml

# 写入记忆
curl -X POST localhost:1934/v1/write \
  -H 'Content-Type: application/json' \
  -d '{"content":"v8 GC 是分代式的","context":{"agent":"assistant"}}'

# 检索
curl 'localhost:1934/v1/search?q=v8+gc&mode=fast'

# 图扩散检索
curl 'localhost:1934/v1/search?q=v8&mode=deep&depth=2&include_graph=true'

# 统计
curl localhost:1934/v1/status
```

## 核心特性

- **双系统检索**: S1 Fast (向量 top-k) + S2 Deep (SQLite CTE BFS 图扩散)
- **Ebbinghaus 遗忘曲线**: 权重指数衰减、访问加成、层级降级 (Full→Summary→Title→Archive)
- **离线记忆巩固**: LLM 摘要生成 + 实体提取 + 自动建边
- **插件化架构**: Embedder / Storage / Consolidator / Forgetter — trait + 配置注入, 独立可替换
- **双存储后端**: SQLite (默认) + LanceDB 磁盘索引 (feature gated)
- **Rust SDK**: `merkur-client` crate, `MerkurClient` trait + `HttpMerkurClient`
- **OpenAPI 3.0**: 多语言 SDK 代码生成

## API

| 方法 | 路径 | 说明 |
|--------|------|-------------|
| `GET` | `/v1/health` | 健康检查 |
| `POST` | `/v1/write` | 写入记忆 |
| `POST` | `/v1/write-batch` | 批量写入 |
| `GET` | `/v1/search` | 检索 (level/category/日期过滤) |
| `GET` | `/v1/memory/{id}` | 获取详情 |
| `PUT` | `/v1/memory/{id}` | 更新 (自动重嵌) |
| `DELETE` | `/v1/memory/{id}` | 删除 (级联边+标签) |
| `GET` | `/v1/status` | 统计 + uptime |
| `POST` | `/v1/consolidate` | 手动合并 |
| `GET` | `/v1/consolidate/log` | 合并审计日志 |
| `POST` | `/v1/forget` | 手动遗忘 |
| `POST` | `/v1/relate` | 建边 |
| `POST` | `/v1/relate-batch` | 批量建边 |
| `GET` | `/v1/graph/{id}` | 图邻域 (含边详情) |

## Docker

```bash
docker build -t merkurdb .
docker run -p 1934:1934 -v ./data:/var/lib/merkur/data merkurdb
```

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
├── server/            # HTTP 服务 + 调度器
└── client/            # Rust SDK
```

## Roadmap

### 已完成 (v0.1.0)

| 类别 | 功能 |
|----------|---------|
| Core | 类型系统 (Memory, Edge, MemoryLevel), 4 个 Plugin Trait, MerkurError |
| Storage | SqliteStorage (WAL + r2d2), InMemoryVectorIndex (cosine similarity) |
| Storage | LanceDbStorage (磁盘 IVF-PQ, feature gated) |
| Embedders | NoopEmbedder (测试), OllamaEmbedder, OpenAIEmbedder (feature gated) |
| Retrieval | S1 Fast (向量 top-k), S2 Deep (CTE BFS 图扩散) |
| Consolidation | NoopConsolidator, LlmConsolidator (LLM 摘要 + 实体提取) |
| Forgetting | EbbinghausForgetter (衰减 + 访问加成 + 级联降级) |
| Scheduler | 后台合并 + 遗忘循环, 手动触发端点 |
| API | 14 个 REST 端点, CORS, 优雅关闭 |
| SDK | `merkur-client` crate: MerkurClient trait + HttpMerkurClient |
| DevOps | Docker, GitHub Actions CI, OpenAPI 3.0 |
| Docs | README + ARCHITECTURE + SPEC + config example |

### 开发中 (v0.2.0)

| 优先级 | 功能 | 说明 |
|----------|---------|-------------|
| P0 | 测试补缺 | LanceDB 测试, LlmConsolidator mock 测试, update_memory 测试 |
| P0 | Prometheus metrics | `/v1/metrics` — 请求数、延迟、记忆统计、合并运行 |
| P1 | 存储统计 | status: 向量索引内存占用、SQLite 文件大小 |
| P1 | 请求限流 | Token bucket, YAML 可配 |
| P1 | 健康详情 | `/v1/health` 带 DB 连接检查、嵌入器探活 |

### 计划中 (v0.3.0+)

| 优先级 | 功能 | 说明 |
|----------|---------|-------------|
| P1 | MCP adapter | Model Context Protocol 集成, Agent 直接接入 |
| P1 | gRPC API | tonic 高性能流式 API, 与 REST 并行 |
| P2 | 静态加密 | SQLCipher 或应用层 embedding 列加密 |
| P2 | 数据库迁移 | Schema 版本管理, `merkur migrate` CLI |
| P2 | PostgreSQL 后端 | PG 存储后端 (通过 Storage trait) |
| P2 | Rust CLI | `merkurctl` — 管理操作 (触发合并、查询状态、备份) |
| P3 | Web Dashboard | Tauri/Yew SPA — 记忆图可视化、配置编辑器 |
| P3 | 多模态 | 图片 embedding 支持 (CLIP 等) |
| P3 | 分布式合并 | 多 worker 并行合并, 大规模记忆库 |

## 文档

- [SPEC_CN.md](docs/SPEC_CN.md) — 设计理念、认知科学背景、产品路线
- [ARCHITECTURE_CN.md](docs/ARCHITECTURE_CN.md) — 技术架构、数据模型、API 规格
- [openapi.yaml](openapi.yaml) — OpenAPI 3.0 完整 spec
- [CHANGELOG.md](CHANGELOG.md) — 变更记录

## License

MIT
