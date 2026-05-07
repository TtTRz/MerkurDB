# MerkurDB — 技术架构

> [English](ARCHITECTURE.md) · v0.1.0

## Crate 结构与依赖

```
crates/
├── core/                # 类型、trait、错误 — 零依赖 (纯定义)
├── storage/             # SqliteStorage + LanceDbStorage
├── embedders/           # NoopEmbedder + OllamaEmbedder + OpenAIEmbedder
├── consolidators/       # NoopConsolidator + LlmConsolidator
├── forgetters/          # EbbinghausForgetter
├── server/              # axum HTTP 服务 + Scheduler
└── client/              # Rust SDK (MerkurClient trait + HttpMerkurClient)
```

**依赖方向**: `core` ← 所有 crate，`server` 依赖所有 crate，`client` 仅依赖 `core`。

## Plugin Trait 体系

4 个核心 trait，通过配置注入实现，彼此独立可替换：

```rust
#[async_trait]
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    async fn encode(&self, text: &str) -> MerkurResult<Vec<f32>>;
    async fn encode_batch(&self, texts: &[String]) -> MerkurResult<Vec<Vec<f32>>>;
}

#[async_trait]
pub trait Consolidator: Send + Sync {
    // 返回报告，由 Scheduler 负责应用 — 不直接写 Storage
    async fn consolidate(&self, memories: &[Memory]) -> MerkurResult<ConsolidationReport>;
}

pub trait Forgetter: Send + Sync {
    fn compute_weight(&self, memory: &Memory, now: DateTime<Utc>) -> f64;
    // now 显式传入以保证测试确定性
    fn decide(&self, memory: &Memory, now: DateTime<Utc>) -> LevelAction;
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String>;
    async fn update_memory(&self, id: &str, content: &str, embedding: Option<&[f32]>) -> MerkurResult<()>;
    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>>;
    async fn delete_memory(&self, id: &str) -> MerkurResult<()>;
    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>>;
    async fn insert_edge(&self, edge: &NewEdge) -> MerkurResult<()>;
    async fn get_edges(&self, memory_id: &str) -> MerkurResult<Vec<Edge>>;
    async fn bfs_expand(&self, seed_ids: &[String], depth: usize, degree_limit: usize) -> MerkurResult<Vec<ScoredMemory>>;
    async fn list_pending(&self, limit: usize) -> MerkurResult<Vec<Memory>>;
    async fn list_for_forgetting(&self, limit: usize) -> MerkurResult<Vec<Memory>>;
    async fn mark_consolidated(&self, ids: &[String]) -> MerkurResult<()>;
    async fn update_level(&self, id: &str, level: i32) -> MerkurResult<()>;
    async fn delete_archived_older_than(&self, days: i32) -> MerkurResult<usize>;
    async fn log_consolidation(&self, started_at: DateTime<Utc>, finished_at: DateTime<Utc>, report: &ConsolidationReport) -> MerkurResult<()>;
    async fn get_consolidation_log(&self, limit: usize) -> MerkurResult<Vec<ConsolidationLogEntry>>;
    async fn stats(&self) -> MerkurResult<StorageStats>;
}
```

## 数据模型

```rust
pub struct Memory {
    pub id: String,
    pub content: String,
    pub abstract_: Option<String>,
    pub category: String,
    pub weight: f64,           // 遗忘曲线权重
    pub level: MemoryLevel,    // Full=2 | Summary=1 | Title=0 | Archived=-1
    pub pending_consolidation: bool,
    pub embedding: Option<Vec<f32>>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub context: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub accessed_at: DateTime<Utc>,
    pub access_count: u64,
}

pub struct Edge {
    pub id: i64,
    pub source_id: String,
    pub target_id: String,
    pub weight: f64,
    pub relation: String,
    pub edge_type: EdgeType,    // Auto (BFS双向) | Manual (BFS有向)
}
```

## 存储层

### SqliteStorage (默认)
- **元数据**: SQLite, WAL 模式, r2d2 连接池 (max 10)
- **向量索引**: `InMemoryVectorIndex` — RwLock<Vec<(id, embedding)>>, cosine similarity
- **启动**: 从 `embedding BLOB` 列加载全量向量到内存
- **表结构**: memories, edges, context_tags, consolidate_log (8 个索引)

### LanceDbStorage (feature gated)
- **元数据**: SQLite (与 SqliteStorage 相同 DDL)
- **向量**: LanceDB 磁盘索引, IVF-PQ, 零拷贝
- **搜索**: LanceDB `nearest_to` 查询, 余弦距离 → 相似度分数转换
- **依赖**: `protoc` (编译时), `--features lancedb`

### 共享 SQL 逻辑
`sqlite_helpers.rs` — 两个后端共享 12 个公共函数 (insert_edge, bfs_expand, search_by_context, stats 等), 消除 ~530 行重复代码。

## 检索系统

### S1 Fast — 向量检索
`Embedder::encode()` → `InMemoryVectorIndex::search()` cosine similarity top-k → SQLite 补全元数据

### S2 Deep — 图扩散
S1 定位种子 → SQLite CTE BFS (递归 WITH RECURSIVE, path 去环):
```sql
WITH RECURSIVE bfs(id, d, w, path) AS (
    SELECT value, 0, 1.0, value FROM json_each('["seed1","seed2"]')
    UNION
    SELECT CASE WHEN e.source_id=bfs.id THEN e.target_id ELSE e.source_id END,
           bfs.d+1, bfs.w*e.weight,
           bfs.path||','||...
    FROM bfs JOIN edges e ON (auto双向 OR manual有向)
    WHERE bfs.d < {depth} AND path NOT LIKE '%'||...||'%'
)
SELECT ... FROM bfs JOIN memories m WHERE bfs.d>0 AND m.level>=0
```

## 认知管线

### 遗忘曲线 (EbbinghausForgetter)
```
w(t) = w₀ · α^(Δt/d) · (1 + β · log₂(1 + n))
```
- α: decay_factor (0.9), d: half_life (86400s), β: access_boost (0.1), n: access_count
- 降级阈值: Full→Summary (w<0.3), Summary→Title (w<0.2), Title→Archive (w<0.1)
- `access_count` 每次 `get_memory` 自动递增

### 合并 (Consolidator)
1. Scheduler 扫描 `pending_consolidation=1` 的记忆
2. Consolidator 分析 → 返回 `ConsolidationReport` (abstracts + edges)
3. Scheduler 应用结果: insert_context_tag + insert_edge + mark_consolidated
4. 写入 `consolidate_log` 审计记录

## 配置规格

```yaml
server:
  host: "127.0.0.1"
  port: 1934

storage:
  type: "sqlite"          # sqlite | lancedb
  sqlite:
    path: "~/.merkur/data/merkur.db"

plugins:
  embedder:
    type: "noop"           # noop | ollama | openai
    noop: { dim: 384 }

consolidation:
  interval_seconds: 60
  batch_size: 10

forgetting:
  interval_seconds: 300
  batch_size: 100
  archive_days: 30
  decay_factor: 0.9
  half_life_seconds: 86400
  access_boost: 0.1
  threshold_to_l1: 0.3
  threshold_to_l0: 0.2
  threshold_archive: 0.1

retrieval:
  fast_default_limit: 10
  score_threshold: 0.3
```

环境变量覆盖: `MERKUR_` 前缀, 优先级: 环境变量 > config.yaml > 默认值。

## API 端点

| 方法 | 路径 | 说明 |
|--------|------|-------------|
| `GET` | `/v1/health` | 健康检查 |
| `POST` | `/v1/write` | 写入单条记忆 |
| `POST` | `/v1/write-batch` | 批量写入 (部分成功时返回 errors 列表) |
| `GET` | `/v1/search` | 语义检索 (level/category/日期/include_graph 过滤) |
| `GET` | `/v1/memory/{id}` | 获取记忆详情 (自动更新 access_count) |
| `PUT` | `/v1/memory/{id}` | 更新记忆内容 (自动重嵌 + 标记待合并) |
| `DELETE` | `/v1/memory/{id}` | 级联删除 (边+标签+向量) |
| `GET` | `/v1/status` | 存储统计 + uptime |
| `POST` | `/v1/consolidate` | 手动触发合并 |
| `GET` | `/v1/consolidate/log` | 合并审计日志 |
| `POST` | `/v1/relate` | 手动建边 |
| `POST` | `/v1/relate-batch` | 批量建边 |
| `POST` | `/v1/forget` | 手动触发遗忘评估 |
| `GET` | `/v1/graph/{id}` | 图邻域 (含 edges 详情) |

错误格式: `{"error": {"code": "...", "message": "..."}}`

## Feature Gates

| Feature | 依赖 | 后端 |
|---------|------|------|
| `ollama` (default) | reqwest | OllamaEmbedder |
| `openai` | reqwest | OpenAIEmbedder |
| `lancedb` | lancedb + arrow + protoc | LanceDbStorage |

```bash
cargo build --features openai,lancedb
```

## 技术选型

| 层 | 选型 | 理由 |
|----|------|------|
| HTTP | axum 0.8 | tokio 生态, async |
| SQLite | rusqlite 0.32 (bundled) | 零系统依赖 |
| 向量 (v0) | 内存 FAISS-like | 万级以内 OK |
| 向量 (v1) | LanceDB 0.27 | 磁盘索引, IVF-PQ |
| 序列化 | serde + serde_json | Rust 标准 |
| 配置 | figment 0.10 | 多层合并 |
| 日志 | tracing | 结构化 |
| 错误 | thiserror 2 | derive macro |
| SDK | OpenAPI 3.0 + Rust trait | 多语言生成 |
| 部署 | 单个 8MB 二进制 + Docker | 零运行时 |

## 项目规模

```
7 crates · 31 Rust 源文件 · ~4,500 行
21 测试 · 0 clippy 警告
14 API 端点 · 3 feature flags
```
