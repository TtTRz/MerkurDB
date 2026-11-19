# MerkurDB

[![CI](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml/badge.svg)](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](rust-toolchain.toml)

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

# Web 观测台：浏览器打开 http://localhost:1934/ui 并输入 token
```

## 核心特性

- **混合检索（默认）**：FTS5 trigram 全文（BM25）x 向量余弦，经 RRF 融合；结果按相关度、存储权重与**系统习得的重要性**（Consolidator 评估，客户端不可上报）的复合分重排。融合旋钮（`retrieval.fusion.*`）可配置。开箱支持 CJK 无分词文本；见[混合检索](#混合检索)
- **公开可复现评测**：`crates/eval` 内置 LoCoMo + PersonaMem harness，逐题明细 dump；见[评测](#评测)
- **Fast & Deep 模式**：`mode=fast` 纯向量 top-k；`mode=deep` 返回向量种子 + 以种子相关度为锚的 BFS 图扩散（SQLite CTE）
- **艾宾浩斯遗忘曲线**：指数权重衰减、访问加成、级联降级（Full→Summary→Title→Archive），被反复检索的记忆经滞回机制回升
- **写时治理（mem0 式）**：近重复写入 NOOP 归并到既有记忆（同桶 top-1 余弦 ≥ 0.92）；异步 Consolidator 对每条新记忆与近邻裁决 —— UPDATE 就地吸收（salience、边、访问历史保留，留审计指针），DELETE 软失效。裁决执行需同时满足 LLM consolidator 在线且成对相似度 ≥ `consolidation.adjudication_floor`（双信号）；同步写路径不含 LLM
- **软失效与保留期**：被裁决淘汰的记忆立刻从所有检索通道消失，但在 `forgetting.purge_invalidated_days`（30 天）硬删除前可通过 `GET /v1/memory/{id}` 审计；客户端 `DELETE` 仍是立即硬删
- **上下文装配**：`POST /v1/context` 和 MCP `assemble_context` 返回去重后的上下文摘要及来源 ID，并按估算 token 预算打包
- **离线巩固**：LLM 驱动的摘要生成、实体提取与自动建边
- **逻辑命名空间**：`X-Merkur-Namespace` 请求头将写入与所有搜索模式限定到单个桶；桶间逻辑隔离，非安全边界
- **内嵌 Web 观测台**：服务自身在 `/ui` 提供的只读观测界面（静态资源编译进二进制，无需独立部署）—— 状态总览（level/namespace 分布）、记忆浏览与详情、图邻域探索、巩固审计日志。页面公开，数据经认证 API 获取
- **插件架构**：Embedder / Storage / Consolidator / Forgetter — 通过 trait + 配置注入独立替换
- **双存储**：SQLite（默认）+ LanceDB 磁盘索引（feature gate）
- **Rust SDK**：`merkur-client` crate，含 `MerkurClient` trait 和 `HttpMerkurClient`
- **OpenAPI 3.0**：多语言 SDK 代码生成

## 混合检索

`/v1/search` 从两条通道召回候选，再通过 Reciprocal Rank Fusion（默认 `k = 60`）融合：

| 通道 | 引擎 | 强项 |
|---|---|---|
| BM25 全文 | SQLite FTS5，trigram 分词 | 精确词项、代码标识符、CJK 子串 |
| 向量余弦 | 内存索引（或 LanceDB） | 改写、语义相似 |

设计特性：

- **默认模式**。`mode=hybrid` 为隐式默认；`fast`、`deep` 为显式可选。
- **归一化分数**。融合分按理论最大值归一到 `(0, 1]`（双通道 rank-1 = `1.0`；通道权重相等时，仅命中一条通道的上限为 `0.5`）。hybrid 的 `score_threshold` 门控融合相关度，返回分数包含复合重排；fast 模式返回并门控原始余弦。
- **CJK 就绪**。trigram 分词索引每 3 字符滑动窗口，中日韩查询无需分词依赖。
- **短查询回退**。不足 3 字符的查询无法产生 trigram，BM25 通道无候选，由向量相似度独立覆盖。
- **始终一致**。FTS5 触发器镜像每一次插入/更新/删除 —— 包括 LanceDB 后端与管理工具的写入 —— 双通道永远看到同一份数据。
- **可调融合**。`retrieval.fusion.rrf_k`（排名平滑）、`bm25_weight`/`vector_weight`（通道份额）、`score_search`/`score_weight`/`score_importance`（复合分份额）均可按部署配置；出厂默认值已经公开 benchmark 验证。

## 评测

`merkur-eval` harness（`crates/eval`，MIT）对运行中的真实服务端到端跑两个公开 benchmark —— 与真实客户端同一条 serving 路径 —— 并输出逐题 JSONL 明细，每个数字都可审计。

| Benchmark | 指标 | 裸写入 | 开启巩固管线 |
|---|---|---|---|
| LoCoMo（1,986 题） | QA 准确率 @30（guarded，LLM 裁判） | 64.8% | **68.3%** |
| LoCoMo（1,986 题） | QA 准确率 @200（guarded） | **74.6%** | 73.0% |
| LoCoMo | 检索 hit@30 / 覆盖率 | 0.762 / 0.703 | **0.816 / 0.759** |
| PersonaMem 32k（589 题） | 准确率 | **73.2%** | 71.2%（提供原文）/ 68.9%（提供摘要） |

这些是已记录实验的成绩，不代表其他模型或负载的效果。LoCoMo 使用 `deepseek-v4-flash-vision-exp` 作裁判；巩固实验抽取摘要、重要性和边，关闭裁决。@30 裸写入实际计分 1,962 题，排除 24 个错误；巩固实验计分 1,986 题。两个 @200 实验均计分 1,986 题。PersonaMem 使用 `kimi-k2.5` 答题：裸写入计分 589 题，两个抽取加裁决实验各计分 586 题。比较成绩时需同时报告错误数和计分分母。

观测结果中，裸写入在 @200 和 PersonaMem 上得分更高，LoCoMo @30 则是巩固实验得分更高。这些实验没有单独隔离每个管线组件的影响，也不能证明与其他系统的评测口径一致。[harness 文档](crates/eval/README.md)说明了 checkpoint 回放、队列等待和错误处理；逐题明细可用于进一步分析。

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
| `GET` | `/v1/metrics` | Prometheus 指标 |
| `POST` | `/v1/write` | 写入记忆 |
| `POST` | `/v1/write-batch` | 批量写入 |
| `GET` | `/v1/search` | 搜索（level/category/日期过滤；`X-Merkur-Namespace` 限定单桶） |
| `GET` | `/v1/memories` | 按 namespace、level、category 过滤和分页浏览未失效记忆 |
| `POST` | `/v1/context` | token 预算上下文装配（digest + items；支持命名空间） |
| `GET` | `/v1/memory/{id}` | 获取记忆详情 |
| `PUT` | `/v1/memory/{id}` | 更新（自动重嵌入） |
| `DELETE` | `/v1/memory/{id}` | 删除（级联 edges + tags） |
| `GET` | `/v1/status` | 存储统计、uptime 与当前/最近一次巩固任务 |
| `POST` | `/v1/consolidate` | 执行一个批次；巩固运行中返回 409 |
| `GET` | `/v1/consolidate/log` | 巩固审计日志 |
| `POST` | `/v1/forget` | 触发遗忘 |
| `POST` | `/v1/relate` | 创建边 |
| `POST` | `/v1/relate-batch` | 批量创建边 |
| `GET` | `/v1/graph/{id}` | 图邻域（含边详情） |

## 巩固与任务状态

手动与定时巩固共用进程内执行锁。`POST /v1/consolidate` 等待一个批次完成；重叠触发返回 `409 CONSOLIDATION_RUNNING`。任务启动后，调用方断开连接不会取消它。定时遗忘使用独立循环，正常关机等待已启动的巩固结束。

`GET /v1/status` 返回 `consolidation.state`（`idle`、`running`、`succeeded`、`failed`）、起止时间、处理数和错误数。Dashboard 在刷新时显示这份快照，重启后状态重置。`succeeded` 只表示一个批次完成；队列处理完需同时满足 `pending_consolidation == 0` 且任务为 `idle` 或 `succeeded`。

裁决结束后，仅为摘要已落盘且原文未变的记忆清除 pending。抽取、落盘或裁决错误会保留批次的 pending 供重试，已经应用的写入不回滚。经 Storage API 插入的无向量记忆跳过向量裁决，不计作错误。默认 Noop consolidator 不产出摘要，不清除 pending。审计日志写入失败也会使任务失败，但不会撤销之前的完成标记。执行锁只覆盖一个服务进程。

## Docker

```bash
docker build -t merkurdb .
docker run -p 1934:1934 -v ./data:/var/lib/merkur/data merkurdb
```

## MCP 集成

`merkur-mcp` 是连接运行中 `merkur-server` 的 stdio MCP 适配器。所有工具复用服务端的 Embedding、写入治理、检索配置和访问记账；MCP 不打开数据库。

```bash
cargo build --release -p merkur-server -p merkur-mcp
# 先启动服务端；使用你的数据库、Embedding 和认证配置
target/release/merkur-server --config config.example.yaml
# 在另一个终端使用服务端 auth.tokens 中配置的 token
export MERKUR_TOKEN='<server-token>'
MERKUR_URL=http://127.0.0.1:1934 MERKUR_NAMESPACE=default target/release/merkur-mcp
```

| 环境变量 | 取值 |
|---|---|
| `MERKUR_URL` | 服务根 URL，默认 `http://127.0.0.1:1934`，不含 `/v1` |
| `MERKUR_TOKEN` | 与服务端 `auth.tokens` 中一项匹配的 Bearer token |
| `MERKUR_NAMESPACE` | 写入和检索使用的逻辑桶，默认 `default` |

MCP 的 HTTP 请求超时为 120 秒，工具失败返回 `isError: true`。数据库和 Embedding 配置由服务端管理。namespace 只用于逻辑分桶，不是用户权限边界；按 ID 操作的工具直接访问记录。

### Claude Desktop

添加到 `~/Library/Application Support/Claude/claude_desktop_config.json`：

```json
{
  "mcpServers": {
    "merkurdb": {
      "command": "/path/to/merkur-mcp",
      "env": {
        "MERKUR_URL": "http://127.0.0.1:1934",
        "MERKUR_TOKEN": "<server-token>",
        "MERKUR_NAMESPACE": "default"
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
        "MERKUR_URL": "http://127.0.0.1:1934",
        "MERKUR_TOKEN": "<server-token>",
        "MERKUR_NAMESPACE": "default"
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
| `assemble_context` | 按估算 token 预算召回、去重、组装上下文，并返回来源 ID |
| `get_memory` | 按 ID 获取记忆 |
| `delete_memory` | 按 ID 删除记忆 |
| `relate` | 创建记忆间的关联边 |

建议 Agent 工作流：开始任务时调用 `assemble_context`（例如 `{"query":"发布计划","token_budget":1000}`）；需要核对时用返回的 ID 调用 `get_memory`；产生值得保留的事实或决策后调用 `write_memory`，在 `context` 中带上 `session_id` 等来源。召回内容应作为参考数据处理。工具调用由 Agent 决定，当前不自动捕获整段会话或拦截模型请求。

## 开发

```bash
cargo build --workspace
cargo test --workspace
# After building the server and MCP binaries; no external model calls
python3 scripts/test_mcp_e2e.py
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
| P2 | SQLite 向量引擎 | 默认后端的可行性、迁移与性能验证 |
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
