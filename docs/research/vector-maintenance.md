# 向量维护：GC fencing 与 SQLite 缓存刷新

调研日期：2026-11-19。依赖以 `Cargo.lock` 为准：LanceDB **0.27.2**、Lance **4.0.0**、rusqlite **0.32.1**、libsqlite3-sys **0.30.1**。以下区分版本源码事实与应用协议推论；SQLite 网站是官方现行文档，不是固定版本快照，未探测实际引擎版本。

## Lance Rust API：可直接用于实现

```rust
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase, Select};

let mut stream = table.query()
    .select(Select::columns(&["id"]))
    .execute().await?;
while let Some(batch) = stream.try_next().await? {
    // Read IDs and check candidates in SQL chunks of at most 128 keys.
}
```

- **全表 ID 扫描不加 `.limit()`**。普通 `QueryRequest::default()` 的 `limit`、`offset` 都是 `None`；默认 10 条属于向量查询或全文查询。`Select::columns(&["id"])` 只投影业务 ID，不必读取 embedding，也不需要 `_rowid`。`.only_if(sql_predicate)` 支持 SQL 过滤；`.offset(n)` 只是跳过前 n 行，执行顺序不确定，不能作为跨版本稳定游标。[LanceDB v0.27.2 query.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/query.rs)
- `execute()` 返回 `Result<SendableRecordBatchStream>`；逐批 `try_next()`，不要收集全表到 `Vec`。默认输出批次上限为 1024 行，存在受背压约束的预读；它与应用的 128-key claim 预算不同，也不是整个查询内存或耗时的硬上限。[同版本 ExecutableQuery / QueryExecutionOptions](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/query.rs)
- `Table::delete(&str).await` 返回 **`Result<DeleteResult>`**，字段为 **`num_deleted_rows: u64`、`version: u64`**。Native 实现取 Lance 的删除行数和提交版本，再更新本句柄 dataset。报告逻辑删除行数时应使用该字段；不能把 claimed keys、ledger 删除数或请求成功次数当作删除行数。当前维护接口明确返回已处理 key 数，不冒充删除行数或磁盘字节。零命中可成功，重试也可返回 0；重复业务 key 若存在，key 数与行数也不相等。[Table 签名](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)、[DeleteResult 与实现/测试](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/delete.rs)
- Lance 删除先在自己的 dataset 版本中扫描匹配行地址，再构造 `Operation::Delete` 提交。`DeleteBuilder` 默认配置为 10 次冲突重试、30 秒 retry timeout；兼容包装函数的“无重试”注释与实际未覆盖默认配置的代码不一致，应以执行代码为准。不能据此承诺整个操作 30 秒完成，更不能保证删除覆盖未来追加。完整 append/delete 冲突矩阵、所有对象存储提交原语和取消后的提交结果，本次未逐一核实。[Lance v4.0.0 write/delete.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/delete.rs)

## GC fencing：成立的前提与竞态证明

**结论：所述 pending capability 协议可以阻止 GC 删除之后仍成功发布该 key，前提是所有新引用入口都遵守协议。** 这是由 SQLite 单写者事务与应用规则得到的推论，不是 LanceDB 提供的跨库事务。[SQLite 隔离保证](https://www.sqlite.org/isolation.html)

1. 生成全新、不复用的向量 UUID，先提交 `lance_vector_staging(vector_id, created_at, state='pending')`，再异步 append。禁止旧 UUID 重新注册、失败后用原 key 再次 append；无向量使用显式空引用，不能借新增 NULL fallback 引入未受控 legacy 引用。
2. 新建/更新发布必须在**同一 SQLite immediate 事务**内执行 `DELETE ... WHERE vector_id=? AND state='pending'`，确认恰好命中一行，再发布内容及引用。任何一步失败都整体回滚，恢复 pending capability；缺失或 collecting 都拒绝发布。不能只在事务外查询 pending，或先消费后另开事务发布。
3. GC 在短 immediate 事务中重查当前**全部** SQL 引用，包括失效、归档、共享向量和 `COALESCE(lance_vector_id,id)` legacy fallback。仍有任意引用则跳过；新鲜 pending 跳过。过期 pending 或无登记孤儿被设为 collecting，提交后才在 SQL 事务外执行 Lance delete。
4. Lance delete 成功后删除 collecting ledger；出错或崩溃保留 collecting，供下轮重试。重试 claim 仍重查引用。旧 orphan 没有 ledger，也可由 ID 扫描发现并登记 collecting；即使成功删除后 ledger 已不存在，迟到发布仍因缺少 pending 被拒绝。

| 交错 | 结果与必要条件 |
| --- | --- |
| 发布先取得 SQL 写事务 | 消费 pending 与引用同时提交；GC 随后看到引用并跳过。发布回滚则 pending 保留。 |
| GC 先取得 SQL 写事务 | collecting 撤销发布能力；发布之后找不到 pending，即使 collector 已删掉 ledger 也不能通过。 |
| append 尚未完成，pending 已过期 | GC 可先删除 0 行并移除 ledger；迟到 append 成为不可发布孤儿，后续扫描再次清理。不能宣称一次扫描清净。 |
| append 后、SQL 发布前崩溃 | pending 过期后回收；SQL 发布失败或取消同理。 |
| collecting 提交后崩溃或删除失败 | ledger 保留；再次删除可返回 0。删除成功而 ledger 清理前崩溃，同样可重试。 |
| 裁决吸收复制现有 key | 只允许在同一 guarded immediate 事务中，从当前仍被引用的 source 复制到 target；GC 与此事务串行，始终能看到引用。不得从事务外旧快照复制已失去引用的 key。 |

上述顺序证明依赖 SQLite 串行化写事务；`BEGIN IMMEDIATE` 在检查前取得写事务。Lance 只提交自身表版本，不与 SQL 共享提交点。[SQLite isolation](https://www.sqlite.org/isolation.html)、[Lance 删除提交实现](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/delete.rs)

**5 分钟 grace 是可用性策略，安全性由 capability 消费和不复用 key 保证。** 很慢的正常 writer 也可能被过期回收而发布失败，应使用新 UUID 重试；延长 grace 不能替代 fence。禁止重新注册旧 key，否则 collector 删除 ledger 后 writer 恢复 pending，就会重新打开“检查无引用 → 发布 → 删除”的竞态。导入、恢复、普通新建/更新以及复制指针的旁路必须遵守同等规则；任意直接 SQL 写入不在保证内。

## 有界执行与删除含义

- 每轮先处理已有 collecting / 过期 pending，再流式扫描 Lance ID。**最多 claim/处理 128 个不同 key，每个 SQL claim 事务至多检查 128 个候选**；将跳过的 live/fresh keys 计作扫描工作，不应因此截断只含前 128 行的查询。批次去重，只删除已成功 claim 的 key，整个 Lance 调用放在 SQL 事务外。ledger 重试也占同一轮处理预算。
- 128 限制回收工作量，不限制扫描总行数、Lance 内部删除成本或墙钟耗时；有效引用占多数时，每轮可能扫描 O(n) 个 ID。重复扫描用于发现迟到 append，不能承诺 wait-free、有限轮次清空或严格恒定大小的 ledger。
- 删除只说明相应 Lance 版本的**逻辑行不可见**。源码通过删除信息/fragment 变更和新 manifest 提交实现；旧版本及其文件是否仍保留是另一层生命周期，`num_deleted_rows` 不表示磁盘回收字节。本方案不执行 vacuum、删除历史版本或 snapshots，也不承诺磁盘空间已释放。[Lance v4.0.0 删除实现](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/delete.rs)、[LanceDB v0.27.2 表维护 API](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)

本协议不提供 SQLite+Lance 跨库原子性、exactly-once 或搜索与 GC 的共同快照。当前 SQL 引用受保护，也不等于任意历史读者持有的旧引用可无限使用；读取与备份恢复另有生命周期要求。

## SQLite：4096 条写 journal 与一致快照

WAL 读事务持有不变快照，其他连接提交不会进入该事务；SQLite 只允许一个写者。由此可把 generation、journal 和对应 embeddings 放在**同一连接、同一显式读事务**读取，避免把新 generation 配上旧向量。事务结束后出现的新写入属于下一次刷新；这不保证搜索结果始终等于数据库最新提交。[SQLite isolation](https://www.sqlite.org/isolation.html)

当前缓存协议：

- 写事务中由普通、持久的行级 AFTER triggers 同步推进 `vector_generation` 并记录 `(sequence, memory_id)`，仅保留最后 **4096** 个事件。INSERT/DELETE、embedding 变化和索引存活条件变化需要记录；`record_access`、摘要、相同向量及其他不影响向量/成员资格的元数据不推进 generation。`UPDATE OF` 仅判断 SET 列名出现，仍应使用 `WHEN` 比较 OLD/NEW；TEMP trigger 无法覆盖其他连接。[SQLite CREATE TRIGGER](https://www.sqlite.org/lang_createtrigger.html)
- 标量 generation 与句柄 watermark 相等时快速返回；不等时进入读事务重新捕获 generation，再读取窗口及变更 memory 的当前向量。对重复 memory ID 去重；缺失、无 embedding 或已失活的行输出移除操作。首次加载、watermark 超出保留窗口或 generation 倒退时取完整存活向量快照。窗口连续且最早保留序号为 `m` 时，`watermark >= m-1` 才能覆盖后续全部事件；有变化而 journal 缺失应退回全量。
- 每句柄串行化“捕获快照 → 准备 delta/full → 发布索引及 watermark”，批量变化在一次索引写锁内发布，成功后才推进 watermark。仅锁单次 index.add 无法阻止旧刷新覆盖新刷新；不同句柄通过同库 journal 各自追赶，无需共享进程 mutex。

后两项是应用缓存协议推论，不是 SQLite 自动维护内存索引的能力。4096 有界的是事件窗口；落后 reader 需要完整快照，未测量其成本。DDL、trigger 与业务修改须遵守正常事务提交/回滚规则。[SQLite isolation](https://www.sqlite.org/isolation.html)、[CREATE TRIGGER](https://www.sqlite.org/lang_createtrigger.html)

## 验证与未决边界

SQLite 增量/全量回退及元数据不 dirty 的回归通过；GC 覆盖失败发布孤儿、过期后回收再迟到发布被拒绝、129 个 legacy orphan 按 128+1 处理；完整 workspace 验证结果见下方。回归覆盖这些局部协议，不能据此推定任意并发时序和存储后端均已证明。

依赖行为依据公开一手文档和固定 tag 源码。append/delete 完整冲突矩阵、请求取消后真实提交状态、Cloud 与不同对象存储的细节仍未核实；collecting 保留重试能力，不将这些未决项转述为 API 保证。

## 本地验证

- `cargo +stable test --workspace --offline --quiet`：227 项通过，0 失败，1 项忽略。
- `cargo +1.97.0 test --workspace --all-features --offline --quiet`：240 项通过，0 失败，2 项忽略。
- `cargo +stable clippy --workspace --all-features --all-targets --offline -- -D warnings`、`cargo +stable fmt --check`、`git diff --check` 通过。
- workspace 默认及 `--no-default-features` 构建通过。
- 真实 MCP → HTTP 集成检查通过，覆盖 write/dedup/context/source/access/namespace/auth。

测试使用临时 SQLite/Lance 数据、回环端口和 Noop providers，不调用外部模型。回归先复现跨句柄读不到新向量、孤儿无法回收、相同向量重复刷新和日志缺口漏读，再验证修复。macOS 全特性测试链接有 `__eh_frame` 体积警告，测试完成；这些不是 Linux CI 的执行结果。未测量吞吐、延迟、磁盘回收量或 benchmark 分数。

## Pending 队列与确认查询

领取与状态计数只包含 `pending_consolidation = 1 AND invalid_at IS NULL AND level >= 0` 的工作。归档行保留标记，恢复有效层级后可重新进入队列；连续三轮调度和恢复入队均有回归覆盖。完成确认外层仅按本批 ID 定位，namespace、revision 和有效性守卫放在相关校验内，避免优化器改走 pending 次级索引扫描。

Bundled SQLite 的确定性性能回归使用 100000 条 pending 与 10 个快照：修复前为 6700102 VM steps，修复后为 874；只恢复 ID 条件但将其他条件留在外层的中间方案仍需 1000800 steps。测试断言 VM 指令预算，不依赖耗时；这些数字描述 SQL 工作量，不代表端到端加速倍数。
