# MerkurDB — 设计 Spec

> [English](SPEC.md) · v1.0

## 1. 定位

MerkurDB 是一个**外挂式、认知科学启发的独立记忆服务**，为 AI Agent 提供长时记忆。

与现有方案的根本区别：

| | 业界 | MerkurDB |
|--|------|----------|
| 哲学 | 工程驱动 — 怎么存得更多、搜得更准 | **认知驱动 — 人脑怎么记，我们就怎么做** |
| 遗忘 | 视为 bug, 尽量避开 | **第一公民 — 有策略地遗忘比记住一切重要** |
| 巩固 | 写入即完成, 不做离线处理 | **核心机制 — 离线摘要、实体提取、关联建图** |
| 检索 | 单一模式 (向量 top-k) | **双系统 — S1 快检索 + S2 图扩散** |
| 架构 | 多数内嵌 Agent 框架 | **外挂独立服务, 不绑定任何框架** |
| 部署 | Python 栈, 依赖复杂 | **单个 Rust 二进制, 零运行时依赖** |

## 2. 背景

### 2.1 竞品分析发现的系统性缺陷

经过对 Zep、Memobase、GraphRAG、Letta、OpenViking 等已有方案的审查：

| 缺陷 | 说明 | 业界状态 |
|------|------|---------|
| 无巩固机制 | 写入即完成, 不做离线压缩/推理 | **无人做** |
| 无遗忘策略 | 要么全存, 要么粗粒度窗口截断 | **无人做** |
| 无双系统检索 | 只用向量 top-k, 没有快/慢分离 | **无人做** |
| 无情境感知 | 检索不看编码时上下文 | 仅 Zep 有时序 |
| 内嵌式架构 | 记忆绑定 Agent 框架, 不可互换 | 部分已外挂 |

### 2.2 认知科学驱动力

每个机制对应已知的人脑记忆模型：

| 机制 | 认知科学依据 | 实现 |
|------|------------|------|
| Ebbinghaus 遗忘曲线 | 记忆强度随时间指数衰减, 重复访问增强 | `Forgetter` trait |
| 记忆巩固 (Consolidation) | 海马体→皮层转移, 离线重组 | `Consolidator` trait |
| 双系统检索 | Kahneman 系统1 (快) / 系统2 (慢) | S1 Fast / S2 Deep |
| 层级降级 | Full → Summary → Title → Archive | `MemoryLevel` 枚举 |
| 情境依赖记忆 | 编码时的 context 影响检索 | context tags + soft filtering |

## 3. 设计原则

- **外挂优先** — 独立 HTTP 服务, 不嵌入任何 Agent 框架
- **认知科学驱动** — 每个机制对应已知的人脑记忆模型
- **模块可拆卸** — 每层可替换实现 (trait + 配置注入), 不强制绑定技术栈
- **遗忘是第一公民** — 有策略地遗忘比记住一切重要
- **零依赖部署** — 单个 Rust 二进制, 通过 Docker 或裸机运行

## 4. 数据流

```
写入:  Agent → POST /v1/write
         ├─→ Embedder Plugin: 生成 embedding
         ├─→ Storage: 存 SQLite + 更新向量索引
         └─→ 返回 { id, status: "ok" }

检索:  Agent → GET /v1/search?mode=fast|deep
         ├─ S1 fast: Embedder → 向量 top-k → SQLite 补充元数据
         └─ S2 deep: S1 种子 → SQLite CTE BFS 图扩散 → 聚合

后台:  Scheduler 定时触发
         ├─→ Consolidator: 扫描 pending → 摘要 + 实体 → 建边
         └─→ Forgetter: 权重衰减 → 降级 → 归档清理
```

## 5. 记忆生命周期

```
写入 (Full, weight=1.0, pending=true)
  │
  ├─→ [Consolidator 处理] → 生成摘要 + 建边 → pending=false
  │
  ├─→ [每次 get_memory 读取] → access_count++, accessed_at 更新
  │
  └─→ [Forgetter 定时评估]
        w(t) = w₀ · α^(Δt/d) · (1 + β · log₂(1+n))
        
        Full (L2)    w < 0.3 → Summary (L1)
        Summary (L1) w < 0.2 → Title (L0)
        Title (L0)   w < 0.1 → Archive (L-1)
        Archive               → 30天后物理删除
```

## 6. 配置驱动

所有插件在启动时通过配置选择, 可替换且不重新编译：

```yaml
plugins:
  embedder:
    type: "ollama"          # ollama | openai | noop
  consolidator:
    type: "noop"            # noop | llm (LLM 需要外部 API)
  forgetter:
    type: "ebbinghaus"      # ebbinghaus | noop
  storage:
    type: "sqlite"          # sqlite | lancedb
```

## 7. SDK 策略

**混合方案**: Rust trait (参考实现) + OpenAPI 3.0 spec (多语言代码生成)

- MerkurDB 维护 `merkur-client` crate (`MerkurClient` trait + `HttpMerkurClient`)
- 提供 `openapi.yaml`, 用户用 openapi-generator 生成 Python/TypeScript/Go SDK
- 第三方可通过 REST API 直接集成

```rust
// Rust 调用示例
let client = HttpMerkurClient::new("http://localhost:1934");
let resp = client.write("hello world", None).await?;
let results = client.search("hello", Some("fast"), Some(10), None).await?;
```

## 8. 阶段路线

### Phase 0 — 已完成
- 工程骨架 + 类型系统 + SQLite 存储 + Ollama/Noop 嵌入器
- HTTP 服务器 (write, search, memory CRUD, status)
- 21 个单元/集成测试

### Phase 1 — 已完成
- S2 Deep Search (CTE BFS)
- Ebbinghaus 遗忘曲线
- LlmConsolidator
- 后台 Scheduler (consolidate + forget)
- 手动触发端点

### Phase 2 — 已完成
- LanceDB 存储后端 (feature gated)
- OpenAI embedder (feature gated)
- Rust SDK (`merkur-client` crate)
- 合并审计日志、图形端点、搜索过滤
- Docker + CI/CD

### Phase 3 — 规划中
- gRPC API (tonic)
- PostgreSQL 后端
- MCP adapter (Agent 协议接入)
- 分布式 consolidation
- Web UI dashboard
- 多模态支持 (图片 embedding)
- 静态加密 (at-rest encryption)
- 请求限流

## 9. 编程语言决策

选择 **Rust 全栈**的理由：

| 考量 | 结论 |
|------|------|
| 部署 | 单个 8MB 二进制, 零运行时依赖 (无 Python/Node) |
| 并发 | tokio async, 无 GIL, 编译期保证并发安全 |
| 安全性 | 编译期内存安全, 减少生产事故 |
| embedding | 调外部 API (Ollama/OpenAI), 业界标准做法 |
| 开发成本 | 比 Python 慢 2-3x, 但 MerkurDB 代码量可控 (~4500 行) |
| AI 生态 | 通过外部 API 调用规避 Rust AI 生态不足 |

## 10. License

MIT
