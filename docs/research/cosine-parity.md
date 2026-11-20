# Cosine 语义与旧 Lance 索引兼容

依赖以 `Cargo.lock` 为准：LanceDB **0.27.2**、Lance **4.0.0**。公式、索引行为与零向量边界依据固定版本公开源码和本地回归；以下区分依赖契约与应用策略。

## 距离、分数与零向量

| 查询路径 | `_distance` 的含义 | 转为 Cosine similarity |
| --- | --- | --- |
| Float32 flat，默认 L2 | `sum((q[i] - x[i])²)`，没有开平方 | 不能仅凭该距离恢复非单位向量的 Cosine |
| flat，显式 Cosine | `1 - dot(q,x)/(norm(q)*norm(x))` | 非零且距离有限时，`(1.0 - f64::from(d)).clamp(-1.0, 1.0)` |
| IVF-PQ，未调用 refine | 量化向量的近似距离 | 即使 metric 正确，也不能直接用作真实 Cosine 阈值 |
| IVF-PQ，`refine_factor(1)` | 对召回候选取回完整向量，再算实际距离并排序 | 返回候选分数可用于阈值；召回仍是 ANN |

依据：[Lance v4.0.0 L2 内核](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance-linalg/src/distance/l2.rs)、[Cosine 内核](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance-linalg/src/distance/cosine.rs)、[LanceDB v0.27.2 查询契约](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/query.rs)。未指定 metric 时，native 若选中了索引会采用其 metric；因此业务入口始终显式指定 Cosine，不能依赖默认值。[Lance v4.0.0 scanner::vector_search](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/scanner.rs)

Cosine 内核计算双方范数，已有非单位向量无需改写或预先归一化。仅在双方都是单位向量时，平方 L2 才满足 `cos = 1 - d/2`；旧的 L2 距离映射不能用于任意历史向量。该关系是公式推导，不是另一个 LanceDB 分数 API。[Cosine 源码](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance-linalg/src/distance/cosine.rs)

**零向量不是 native Cosine 的合法方向。** 0.27.2 的公开 API 明确说明：一侧或双方全零时距离未定义，这些向量可能永不返回。直接内核没有将其定义为 similarity 0 的分支；零范数可产生 NaN，`KNNVectorDistanceExec` 随后过滤 NaN 距离。本地 flat 实测：5 个候选、limit 10，只返回 4 个，零候选被省略；不能靠转换返回结果中的 NaN 补回该行。[版本 API 契约](https://docs.rs/lancedb/0.27.2/lancedb/enum.DistanceType.html#variant.Cosine)、[v4.0.0 KNN 执行器](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/io/exec/knn.rs)

**本项目兼容策略，非 native 保证：** 延续 SQLite 的 f64 Cosine helper，任一操作数为零时得分 0。零查询流式投影 ID 并返回 0；非零查询出现返回数少于 `min(limit, native_rowcount)`、任意负分或非有限分数时，改为流式读取原始向量并维护有界 top-k。原因是被省略的零向量得分 0，应能排在负分前面；正常分支只保留足量、有限、非负的 native 候选。候选数量检查与 rowcount 应使用相同数据范围，不能拿 SQL 存活行数替代 native 行数；跨调用并发变化不构成共同快照保证。内存有界不代表扫描成本或耗时有界，fallback 仍需遍历数据。

## refine(1) 与旧 L2 索引查询

`VectorQuery::refine_factor(u32) -> Self` 的文档明确区分 `Some(1)` 与 `None`：前者仍取回未压缩向量并重算实际距离。Lance 4 的 `vector_search` 按 `q.refine_factor.is_some()` 执行 `take(... vector_projection)` → `flat_knn(...)`，不是按 factor 是否大于 1 判断。ANN 候选预算是 `k * refine_factor.unwrap_or(1)`；factor 1 没有额外扩大候选集，但确实重算分数。factor 0 在使用索引时被拒绝。[固定版本 query.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/query.rs)、[固定版本 scanner.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/scanner.rs)

query.rs 的公开文档要求查询与索引 metric 一致。**锁定版本的 native 实现有更明确的防护：** 找到列上第一个索引，打开它并读取 `metric_type()`；显式请求 Cosine 而索引为 L2 时，记录警告并退回 brute-force Cosine。它既不静默沿用 L2，也不单因 metric 不同而报错；但打开旧索引本身可以失败，且不会继续搜索同列后面的 Cosine 索引。此结论限于所核对的单向量 native 路径。[Lance v4.0.0 vector_search](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/scanner.rs)

可直接使用的绕过 API：

- LanceDB Rust：`table.query().nearest_to(query)?.distance_type(DistanceType::Cosine).refine_factor(1).bypass_vector_index()`；`VectorQuery::bypass_vector_index(self) -> Self` 将 `request.use_index = false`，承诺 exhaustive flat search。
- 底层 Lance：`Scanner::distance_metric(MetricType::Cosine)` 与 `Scanner::use_index(false)`，均修改 scanner 并返回 `&mut Self`。

依据：[0.27.2 VectorQuery](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/query.rs)、[4.0.0 Scanner](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/scanner.rs)。不要启用 `fast_search()`：它跳过未索引数据，缺少可用索引时 native 可直接返回空结果。

本项目采用保守规则：目标 `vector` 列没有已确认的 Cosine 向量索引，或其任一向量索引 metric 缺失、未知、非 Cosine，就显式 bypass；只有全部相关索引 metric 均已确认 Cosine 才允许 ANN。标量索引不参与此判断。这样保留旧向量和 L2 索引期间即可正确查询；多 metric 混存也不会依赖 native 的第一个索引选择。数据在元数据检查后并发变化时，native 的 mismatch fallback 仍是已核对的防护，但不把这描述为应用拥有原子查询快照。

## 元数据、显式建索引与调度

精确 Rust API：

- `Table::list_indices(&self) -> Result<Vec<IndexConfig>>`；`IndexConfig` 公开字段为 `name: String`、`index_type: IndexType`、`columns: Vec<String>`，**没有 metric 字段**。
- `Table::index_stats(&self, index_name: impl AsRef<str>) -> Result<Option<IndexStatistics>>`；不存在返回 `None`，其他错误仍需处理。用 `distance_type: Option<DistanceType>` 判断 metric；`num_indexed_rows: usize`、`num_unindexed_rows: usize` 描述覆盖范围，另有 `index_type`、`num_indices`、`loss`，不是可靠的 Cosine 匹配替代项。
- native `list_indices` 会读取索引统计，统计失败或字段解析失败的条目可能被跳过；空列表不证明物理索引绝不存在。`index_stats` 也不是零成本字段读取；不应未经测量宣称逐查询检查没有开销。

依据：[0.27.2 index.rs 定义](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/index.rs)、[0.27.2 table.rs 签名与 native 实现](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)。这些元数据 API 已通过编译和运行验证。

显式 Cosine IVF-PQ 的最小调用：

```rust
use lancedb::{DistanceType, index::{Index, vector::IvfPqIndexBuilder}};

table.create_index(
    &["vector"],
    Index::IvfPq(
        IvfPqIndexBuilder::default().distance_type(DistanceType::Cosine),
    ),
)
.execute()
.await?;
```

`IvfPqIndexBuilder::distance_type(self, DistanceType) -> Self` 由宏提供；builder 默认 metric 仍为 L2，必须显式覆盖。`num_partitions(u32)`、`num_sub_vectors(u32)`、`num_bits(u32)` 是可用 setter，本任务无需另调参数。`Index::Auto` 在此版本的浮点向量列上硬编码 L2 IVF-PQ，不会自动推断业务 Cosine；table.rs 自带测试也断言 Auto 的 `stats.distance_type == Some(L2)`。[vector.rs builder](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/index/vector.rs)、[table.rs Auto 分支与测试](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)

`IndexBuilder` 默认 **`replace(true)`、`train(true)`**。可用 `.name(existing.name.clone()).replace(true)` 替换同名旧 L2 索引；`.replace(false)` 遇到同列同名索引会报错，即使其已过时，也不是“存在则无事发生”。替换索引无需重写向量；不要另起一个 Cosine 名称并假定 native 会挑中它。native 创建过程会等待 builder 完成后更新自身 dataset 句柄。[IndexBuilder 默认值与 setter](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/index.rs)、[native create_index](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)

当前应用调度：初始化已有表后异步触发一次检查，插入后沿用触发入口；在本地 guard 内重新打开表并查询元数据。已有已确认的 Cosine 索引则跳过训练；无匹配且行数达到现有 256 策略阈值时显式创建 Cosine IVF-PQ，以默认名称替换受管理的旧 L2 索引。256 是应用策略，不是“所有向量与训练参数必定成功”的 API 保证。元数据错误会使检查失败并记录日志，后续检查可重试。

已有匹配索引但存在未索引追加行时，不在每次 insert 后重新训练：默认查询组合 indexed 搜索和 unindexed flat 搜索，新增行在维护前仍可检索。覆盖检查与后续 optimize 属于独立维护策略。`AtomicBool` 只约束同一共享 flag 的任务，不协调不同 storage 句柄、进程或重启；检查后创建仍可能竞态。后台任务取消或 panic 后的 guard 复位、并发创建冲突和任意版本交错未做完整故障注入；自定义多索引布局使用安全查询回退，不自动删除其他索引。[0.27.2 索引维护契约](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)

## 实现验证

公开 Storage API 的跨后端对照覆盖非单位向量、查询缩放 1/7、零查询、零候选优于负分、cosine 0.85 不通过 0.92 去重，以及缩放后的 0.98 仍能去重。真实 IVF-PQ fixture 验证旧 L2 安全绕过、受管理的默认索引迁移为 Cosine、完整向量精排分数，以及新增行在维护前可检索且再次检查不产生新表版本。浮点对照容差为 `1e-5`，不承诺逐位一致。

- 默认 workspace 测试：227 项通过，0 失败，1 项忽略。
- 全特性 workspace 测试：240 项通过，0 失败，2 项忽略。
- 全特性、全 targets Clippy（`-D warnings`）、`cargo +stable fmt --check` 和 `git diff --check` 通过。

该修复不调整 schema 或改写旧向量。验证使用临时本地数据，不调用在线模型。ANN 召回仍近似；任意非有限输入、极端浮点溢出/下溢和所有原生索引类型未全部覆盖。未测量 benchmark、吞吐或延迟；精排、元数据检查和 fallback 均有额外工作。macOS 全特性链接有 `__eh_frame` 体积警告，测试完成；以上为本地验证结果。
