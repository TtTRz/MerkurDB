# MerkurDB

[![CI](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml/badge.svg)](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92+-orange.svg)](rust-toolchain.toml)

> [English](README.md)

面向 AI Agent 的独立认知记忆服务。灵感源自神经科学，使用 Rust 构建。

单一二进制，零运行时依赖。支持混合检索、图扩散、记忆巩固和艾宾浩斯遗忘曲线。

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

# 搜索（混合 BM25 x 向量融合 —— 默认模式）
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8+gc'

# 低延迟纯向量搜索
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8+gc&mode=fast'

# 图扩散搜索
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8&mode=deep&depth=2&include_graph=true'

# 健康检查（无需认证）
curl localhost:1934/v1/health
```

## 核心特性

- **混合检索（默认）**：FTS5 trigram 全文（BM25）x 向量余弦，经 RRF 融合；结果按相关度、存储权重与**系统习得的重要性**（Consolidator 评估，客户端不可上报）的复合分重排。融合旋钮（`retrieval.fusion.*`）可配置。开箱支持 CJK 无分词文本；见[混合检索](#混合检索)
- **公开可复现评测**：`crates/eval` 内置 LoCoMo + PersonaMem harness，逐题明细 dump；见[评测](#评测)
- **Fast & Deep 模式**：`mode=fast` 纯向量 top-k；`mode=deep` SQLite CTE BFS 图扩散
- **艾宾浩斯遗忘曲线**：指数权重衰减、访问加成、级联降级（Full→Summary→Title→Archive），被反复检索的记忆经滞回机制回升
- **写时治理（mem0 式）**：近重复写入 NOOP 归并到既有记忆（同桶 top-1 余弦 ≥ 0.92）；异步 Consolidator 对每条新记忆与近邻裁决 —— UPDATE 就地吸收（salience、边、访问历史保留，留审计指针），DELETE 软失效。裁决执行需同时满足 LLM consolidator 在线且成对相似度 ≥ `consolidation.adjudication_floor`（双信号）；同步写路径不含 LLM
- **软失效与保留期**：被裁决淘汰的记忆立刻从所有检索通道消失，但在 `forgetting.purge_invalidated_days`（30 天）硬删除前可通过 `GET /v1/memory/{id}` 审计；客户端 `DELETE` 仍是立即硬删
- **上下文装配**：`POST /v1/context` 从混合召回打包出 token 预算内、去重、可直接拼 prompt 的摘要 —— MCP 友好的入口
- **离线巩固**：LLM 驱动的摘要生成、实体提取与自动建边
- **逻辑命名空间**：`X-Merkur-Namespace` 请求头将写入与所有搜索模式限定到单个桶；桶间逻辑隔离，非安全边界
- **插件架构**：Embedder / Storage / Consolidator / Forgetter — 通过 trait + 配置注入独立替换
- **双存储**：SQLite（默认）+ LanceDB 磁盘索引（feature gate）
- **Rust SDK**：`merkur-client` crate，含 `MerkurClient` trait 和 `HttpMerkurClient`
- **OpenAPI 3.0**：多语言 SDK 代码生成

## 混合检索

`/v1/search` 并行跑两个通道，以 Reciprocal Rank Fusion（`k = 60`，业界标准值）融合：

| 通道 | 引擎 | 强项 |
|---|---|---|
| BM25 全文 | SQLite FTS5，trigram 分词 | 精确词项、代码标识符、CJK 子串 |
| 向量余弦 | 内存索引（或 LanceDB） | 改写、语义相似 |

设计特性：

- **默认模式**。`mode=hybrid` 为隐式默认；`fast`、`deep` 为显式可选。
- **归一化分数**。融合分按理论最大值归一到 `(0, 1]`（双通道 rank-1 = `1.0`；单通道命中上限约 `0.5`）。`score_threshold` 在 hybrid 模式门控融合相关度、fast 模式门控原始余弦 —— 跨模式语义一致。
- **CJK 就绪**。trigram 分词索引每 3 字符滑动窗口，中日韩查询无需分词依赖。
- **短查询回退**。不足 3 字符的查询无法产生 trigram，BM25 通道无候选，由向量相似度独立覆盖。
- **始终一致**。FTS5 触发器镜像每一次插入/更新/删除 —— 包括 LanceDB 后端与管理工具的写入 —— 双通道永远看到同一份数据。
- **可调融合**。`retrieval.fusion.rrf_k`（排名平滑）、`bm25_weight`/`vector_weight`（通道份额）、`score_search`/`score_weight`/`score_importance`（复合分份额）均可按部署配置；出厂默认值已经公开 benchmark 验证。

## 评测

`merkur-eval` harness（`crates/eval`，MIT）对运行中的真实服务端到端跑两个公开 benchmark —— 与真实客户端同一条 serving 路径 —— 并输出逐题 JSONL 明细，每个数字都可审计。

| Benchmark | 指标 | MerkurDB | 参照 |
|---|---|---|---|
| LoCoMo（1,986 题） | QA 准确率（LLM 裁判） | **64.8%** | mem0 论文 66.9%（GPT-4 级答题 + 完整抽取管线） |
| LoCoMo | 检索 hit@30 / 覆盖率 | **0.762 / 0.703** | — |
| PersonaMem 32k（589 选择题） | 准确率 | **73.2%** | 前沿 LLM 全上下文 ~52%；TencentDB Agent Memory 76.1%（同答题模型，完整管线） |

测量条件：原始对话轮次直接写入（未开巩固管线）+ 轻量答题模型（LoCoMo 裁判 `deepseek-v4-flash-vision-exp`，PersonaMem 答题 `kimi-k2.5`）；裁判/答题模型差异使跨论文数字为近似对比。harness 设计：免 LLM 的检索召回轨（对照 evidence 标注评分）、裁判打分的 QA 轨（对抗题拒答判对）、PersonaMem 的 in-situ checkpoint 回放（无未来信息泄漏）。

```bash
scripts/fetch_locomo.sh          # 数据集（CC BY-NC / MIT，gitignored）
scripts/run_locomo.sh            # ingest + recall + qa，一次性 server
scripts/run_personamem.sh        # PersonaMem in-situ 回放
scripts/sweep_fusion.sh          # P1-5 融合参数扫描（持久语料库）
```

## API

| 方法 | 路径 | 描述 |
|------|------|------|
| `GET` | `/v1/health` | 健康检查 |
| `POST` | `/v1/write` | 写入记忆 |
| `POST` | `/v1/write-batch` | 批量写入 |
| `GET` | `/v1/search` | 搜索（level/category/日期过滤；`X-Merkur-Namespace` 限定单桶） |
| `POST` | `/v1/context` | token 预算上下文装配（digest + items；支持命名空间） |
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
| `search_memory` | 混合 BM25 + 向量相关度搜索 |
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
├── client/            # Rust SDK
├── cli/               # merkurctl 管理 CLI
├── mcp/               # MCP 服务器（stdio）
└── eval/              # LoCoMo + PersonaMem benchmark harness
```

## 路线图

### 规划中 (v0.6.0+)

| 优先级 | 特性 | 描述 |
|--------|------|------|
| P2 | 静态加密 | SQLCipher 或应用层 embedding 列加密 |
| P2 | PostgreSQL 后端 | 通过 Storage trait 的 PG 存储后端 |
| P3 | 多模态 | 图像嵌入支持（CLIP 等） |

已发布各版本的完整变更记录见 [CHANGELOG.md](CHANGELOG.md)。

## 文档

- [SPEC_CN.md](docs/SPEC_CN.md) — 设计哲学、认知科学背景、产品路线
- [ARCHITECTURE_CN.md](docs/ARCHITECTURE_CN.md) — 技术架构、数据模型、API 规范
- [openapi.yaml](openapi.yaml) — OpenAPI 3.0 规范
- [CHANGELOG.md](CHANGELOG.md) — 变更日志

## 许可证

MIT
