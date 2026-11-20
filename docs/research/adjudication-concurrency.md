# Adjudication 并发安全与响应兼容性研究

调研日期：2026-11-19。范围：官方文档、版本源码及仓库实现核对；未调用在线模型或测量性能。本文区分依赖保证、设计推论和回归验证。

## 结论

采用 **SQLite 的 revision 守卫事务作为裁决提交点，Lance 向量采用不可变 key**。普通 Lance 写入先追加新向量，再由 SQL 发布引用；吸收裁决直接在一个 SQL 事务内复制当前 source 的内容和向量引用到 target，并使 source 失效，无需再次写 Lance，也不依赖事务外提前读取的 embedding。

这能把内容、向量引用及 source 失效的业务可见性统一到 SQLite 提交点。它是应用层发布协议，**不是 SQLite+LanceDB 分布式事务，也不承诺 exactly-once**。协议必须覆盖全部写入与检索路径；覆盖不完整时，guarded UPDATE 应显式拒绝，保留 pending，不能用普通更新接口降级执行。当前 schema v8 的发布资格、孤儿 GC 与缓存刷新见[向量维护协议](vector-maintenance.md)。

## 版本与一手来源

`Cargo.lock` 锁定 rusqlite **0.32.1**、libsqlite3-sys **0.30.1**、LanceDB **0.27.2**、Lance **4.0.0**、Serde **1.0.228**；`crates/storage/Cargo.toml` 启用了 SQLite bundled。本文不根据包装库版本推断实际 SQLite 引擎版本，未运行版本探测。

| 来源 | 已核实内容 |
| --- | --- |
| [SQLite isolation](https://www.sqlite.org/isolation.html) | 独立连接、线程和进程之间的事务隔离；单写者；WAL 快照及 `SQLITE_BUSY_SNAPSHOT` |
| [SQLite transactions](https://www.sqlite.org/lang_transaction.html) | 默认 deferred；`BEGIN IMMEDIATE`；事务提交与回滚、busy 边界 |
| [SQLite UPDATE](https://www.sqlite.org/lang_update.html)、[changes](https://www.sqlite.org/c3ref/changes.html) | WHERE 条件更新、零行匹配及实际影响行数 |
| [rusqlite v0.32.1 transaction.rs](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/transaction.rs)、[statement.rs](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/statement.rs)、[busy.rs](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/busy.rs) | 事务 API、RAII、行数返回与 busy timeout |
| [LanceDB v0.27.2 update.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/update.rs)、[add_data.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/add_data.rs) | `only_if`、`UpdateResult`、默认 append 与 `AddResult` |
| [LanceDB v0.27.2 connection.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/connection.rs)、[dataset.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/dataset.rs)、[table.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs) | OSS 跨实例读刷新、缓存与 `checkout_latest` |
| [Lance v4.0.0 update.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/update.rs)、[retry.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/retry.rs)、[commit.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance-table/src/io/commit.rs) | 带版本的表提交、冲突重试及条件创建 manifest |
| [Serde container attributes](https://serde.rs/container-attrs.html)、[field attributes](https://serde.rs/field-attrs.html)、[default](https://serde.rs/attr-default.html)、[v1.0.228 派生源码](https://github.com/serde-rs/serde/blob/v1.0.228/serde_derive/src/de.rs) | JSON 未知字段与新增缺省字段 |

SQLite、Serde 网站是查阅时的官方文档，并非版本快照；Rust API 行为优先依据上述 tag 源码。Lance [事务规范](https://lance.org/format/table/transaction/)可辅助理解，但不把当前规范中较新的能力直接归给 4.0.0。

## SQLite 与 rusqlite：保证和限制

SQLite 对同一个数据库的写事务进行串行化，隔离覆盖同进程独立连接和不同进程；正常数据库文件锁是这一保证的前提。进程内 mutex 只协调持有同一把锁的调用者，不能替代数据库隔离，也不能自动覆盖独立的 `SqliteStorage::new` 句柄。[官方隔离说明](https://www.sqlite.org/isolation.html)

`BEGIN IMMEDIATE` 在读取待修改行之前获取写事务，其他写者必须等待或失败。它适合短暂的“读当前 source/target → 校验 → 两行修改 → 提交”，不应包围 LLM、embedding 或 awaited Lance 写入。WAL 允许读者继续读取旧快照；已过期的读事务升级写入可能得到 `SQLITE_BUSY_SNAPSHOT`，加长 timeout 不会使旧快照变新。[事务说明](https://www.sqlite.org/lang_transaction.html)、[隔离说明](https://www.sqlite.org/isolation.html)

rusqlite 0.32.1 的 `Connection::transaction()` 默认 deferred；应显式使用 `transaction_with_behavior(TransactionBehavior::Immediate)`。事务绑定一条连接，默认 drop 时回滚；只有 `commit()` 成功后才报告 applied。`commit(self)` 消耗事务，错误不能当成提交成功；drop 的清理错误不会自动上报。连接池辅助函数若重新获取连接，就不属于该事务。[版本源码](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/transaction.rs)

条件更新的零行匹配不是 SQL 错误，必须检查返回行数；`Statement::execute` 返回 `sqlite3_changes` 对应的直接影响行数，不包含触发器引起的附带变化。两个必要修改必须各命中一行，否则整体回滚。[SQLite UPDATE](https://www.sqlite.org/lang_update.html)、[changes](https://www.sqlite.org/c3ref/changes.html)、[rusqlite statement.rs](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/statement.rs)

busy 是操作错误或有限重试条件，不等同业务 revision 冲突。若重试短 SQL 事务，每次仍必须对照原裁决快照，不能刷新 expected revision 后重放旧裁决。0.32.1 新连接的默认 busy timeout 在源码中记为 5000ms；仓库可覆盖它，不应据此声称请求不会超时。[busy.rs](https://github.com/rusqlite/rusqlite/blob/v0.32.1/src/busy.rs)

## LanceDB 0.27.2：能做什么，不能做什么

`table.update().only_if(predicate).column(name, expr).execute().await` 支持 Lance 表内的条件更新，返回 `UpdateResult { rows_updated, version }`；并非返回裸行数。底层扫描条件匹配的行，产生数据文件，再提交 Lance 表版本。这个 predicate 不能读取 SQLite 的 pending 状态，也不能把 source 的 SQL 失效操作纳入提交。[LanceDB update.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/update.rs)、[Lance update.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/update.rs)

Lance 4.0.0 更新遇到 `RetryableCommitConflict` 时，可刷新到最新 dataset 并重新执行，包括再次扫描条件；默认更新冲突重试参数为 10 次、30 秒。源码明确首轮操作不受该重试 timeout 包围，因此不能把 30 秒写成整个请求的硬上限。这些机制保护 Lance 的表提交，不证明业务快照或跨存储原子性。[update.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/update.rs)、[retry.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance/src/dataset/write/retry.rs)

版本源码的默认提交处理器根据存储类型选择；其中 `ConditionalPutCommitHandler` 用 `PutMode::Create` 创建 manifest，并把 already-exists/precondition 错误映射为提交冲突。保证依赖底层存储与处理器支持相应原子原语，不能泛化到任意自定义对象存储。该文件开头存在与实际选择逻辑不一致的旧说明，判断应以 `commit_handler_from_url` 实现为准。[Lance commit.rs](https://github.com/lance-format/lance/blob/v4.0.0/rust/lance-table/src/io/commit.rs)

OSS 默认不检查其他实例的新版本；`read_consistency_interval(Duration::ZERO)` 在每次读取时检查，非零值允许暂时旧读。`checkout_latest().await` 可显式刷新。强读检查不锁住随后操作，也不与 SQL 建立共同快照。设置只针对 OSS，不能推广到 Cloud。[connection.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/connection.rs)、[dataset.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/dataset.rs)、[table.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table.rs)

`table.add(...).execute().await` 默认 `AddDataMode::Append`，返回包含 commit version 的 `AddResult`。追加 API 足以实现新 key 的暂存；它没有提供业务主键唯一约束的承诺，必须由应用保证 key 不复用。[add_data.rs](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/add_data.rs)

## 哪些操作不具备跨存储原子性

当前架构的两个后端都使用 SQLite 元数据；SQLite 后端的 embedding blob 与元数据同库，Lance 后端的向量在 Lance 表中。两次独立调用 `update_memory(target, ...)`、`invalidate_memory(source, target)` 不能构成一次吸收。

| 做法 | 失败窗口 |
| --- | --- |
| 先修改 Lance，再修改 SQL | Lance 成功后 SQL 校验、提交失败或进程退出，已修改向量无法由 SQL rollback 撤回 |
| 先修改 SQL，再修改 Lance | 已发布的新内容可能仍匹配旧向量，或向量根本不存在 |
| 持有 SQL 写事务等待 Lance | 只延长 SQL 锁时间；Lance 已提交而 SQL 回滚的窗口仍在 |
| 写失败后按旧值补偿 | 补偿本身可失败，也可能覆盖第三方的新修改 |

以上是两个独立提交机制的工程推论：SQLite 的原子提交范围是其事务数据，Lance 更新提交的是自己的表版本。没有共同提交协议，不能宣称原子双写。[SQLite atomic commit](https://www.sqlite.org/atomiccommit.html)、[LanceDB 更新实现](https://github.com/lancedb/lancedb/blob/v0.27.2/rust/lancedb/src/table/update.rs)

## 当前实现

### 1. 内部 revision 与快照

正文使用独立版本；abstract、importance 更新也会修改 `updated_at`，因此时间戳不适合作为正文版本。存储内部使用单调 `content_revision`，使用 `MemorySnapshot { id, namespace, content_revision }`，不把墙钟时间当 CAS token。

所有影响裁决输入或向量一致性的用户编辑都必须推进 revision，包括内容替换、向量引用替换；作用域和生命周期变化必须由 revision 或事务内显式谓词覆盖。abstract、importance 等不参与裁决语义的系统更新可不推进此 revision。若它们也会影响裁决，须纳入守卫。物理删除后禁止复用逻辑 ID，或另外保留 generation，避免删后重建导致 ABA。

这是应用协议要求，SQLite 不会自动替应用维护 revision。只比较 content 和 `updated_at` 不能排除同时间戳变化或“改后恢复”的 ABA。

### 2. 一个 guarded mutation 入口

`apply_adjudication` 接收 `Absorb { pending, target }` 或 `Invalidate { pending, candidates, target_id }`。入口负责在同一连接的 immediate 事务中：

1. 读取当前 pending 与 target/candidates，验证所有输入快照仍存在、仍有效、namespace 一致、revision 等于裁决时快照；pending 仍具备待裁决资格。候选快照不能为空或包含 pending 自身。
2. 验证 target ID 对应裁决选中的 candidate，拒绝自身吸收、越作用域和不完整快照。
3. Absorb 从**事务内当前 source 行**复制 content，以及 SQLite embedding blob 或 Lance 向量引用到 target；推进 target revision，并使 source 失效。两者同时提交或同时回滚。
4. Invalidate 只使裁决指定的 target 失效。删除指定候选时验证 pending 与该候选；删除 pending 自身时验证模型看到的全部候选。裁决只有失效对象，无法推定首候选就是矛盾方；任一输入被修改、删除、失效或归档，都保留 pending 重试。全部验证和失效在同一事务内完成，不能在事务外分次检查。
5. mark-complete 同样用 pending revision、有效性和 pending 标志作条件；并发用户编辑后的新内容不能被旧裁决标成处理完成。

四路 LLM 并发可保留，但逐条应用始终使用各自**原始**快照。首条吸收推进 target revision 后，后续共享旧 target 快照的 verdict 必须 conflict；不能把新 revision 回填给尚未应用的旧 verdict。同样，source 被另一操作编辑、删除、失效或消费后，本次裁决必须无副作用退出。

### 3. Lance 不可变向量引用

增加 SQLite 内部字段 `lance_vector_id`；有效引用可表示为 `COALESCE(lance_vector_id, id)`，兼容已有按逻辑 ID 存储的 Lance 行。新 key 使用应用生成的全新、不复用 UUID；不把 Lance 内部 `_rowid` 当持久业务引用。

创建先暂存后插入；普通编辑保留显式覆盖语义，以 SQL immediate 事务发布新内容和引用。只有异步裁决使用原始 revision 守卫；普通编辑不复用 LLM 快照。发布顺序如下：

1. 先提交 fresh key 的 pending 发布资格，再在 SQL 事务外追加向量，等待 Lance 成功提交；不得原地修改已发布 key。
2. 开启短 SQL 事务，消费仍有效的 pending 发布资格并原子发布内容、新向量引用和新 revision；异步裁决额外校验原始双方 revision。
3. 追加失败时 SQL 不变；SQL CAS 失败时不发布引用，留下未引用向量。结果必须明确，不得继续吸收 pending。

**吸收无需 stage 新向量**：SQL 双快照守卫通过后，把 source 当前有效向量引用复制给 target，与内容复制及 source 失效同事务提交。source、target 可共享一个不可变向量；source 失效不代表该向量可删除。该设计避免从事务外 embedding 读取结果再次写向量产生的新竞争窗口。

这项可行性判断是由 append API 与 SQL 原子提交组合得到的工程推论，并非 LanceDB 宣布提供的跨库特性。它要求普通更新、导入/覆盖及其他写入口全部遵守不可变 key 协议；不能混用旧的就地更新、按逻辑 ID 删除向量的路径。

### 4. 读取与回收必须一起对齐

- **向量搜索**：Lance 返回向量 key 后，依据 SQLite 当前有效引用映射到所有仍存活、namespace 合法的逻辑 memory；丢弃未发布、过期和失效引用。共享 key 可能对应多个有效 memory，不能假设一对一，也不能把向量 key 直接当作 memory ID。
- **embedding 读取**：从 SQLite 读取当前引用，再到 Lance 查询该 key；禁止继续走 SQLite embedding helper。缺向量、维度异常或重复 key 不能解释为“可以复用旧向量”。若读取后要执行修改，仍需 revision 守卫，或重验引用与 revision。
- **跨实例新版本**：使用 OSS 的零间隔检查或显式最新刷新。默认缓存可能遗漏已发布向量；发现缺失时失败或返回缺失，不能退回过期 key。SQL 检查发生在 Lance 搜索之后，可过滤旧候选，但不能补回旧快照未搜出的新向量。
- **召回完整性**：未发布/旧 key 会占据向量 top-k 名额。后过滤能保证不返回错误版本，却不保证填满结果或维持召回；需要有限补取或接受并明确这一限制。未经测量不声称追加方案无性能代价。
- **引用与孤儿回收**：回收必须检查所有剩余 SQL 引用，包括 legacy fallback，不能删除其他 memory 仍引用的共享 key。后台扫孤儿时，一次“当前零引用”检查不足以排除追加后尚未发布的向量。schema v8 用 pending 发布资格与 collecting 领取状态解决该竞态；具体机制及逻辑删除/MVCC 边界见[向量维护协议](vector-maintenance.md)。

只要所有读取经过 SQL 当前引用与存活过滤，SQL 提交就是业务发布点。任意跨库同时快照、全局搜索线性一致性、外部直接读 Lance、跨系统备份恢复一致性，均不在此保证内。

### 5. Fail-closed 与结果可观测性

| 情况 | 建议结果与副作用 |
| --- | --- |
| revision、存活、namespace 或 pending 资格不匹配 | `conflict`，两行均不修改；可供后续重新取快照裁决 |
| 已声明的向量缺失、解析失败、SQL/Lance 写入错误 | `failed`，不宣称 applied，不消费 pending；合法无向量记忆跳过向量裁决 |
| 不可变 key 协议尚未覆盖全部 Lance 路径 | `unsupported` guarded UPDATE；原样保留 pending，明确 reason |
| guarded DELETE | 在同一 SQL 事务内验证来源与全部所需证据后失效 target；物理向量清理延后 |
| 条件 mutation 提交成功 | `applied`；提交后再更新或失效内存索引缓存 |

`unsupported` 是显式能力限制，不是无限静默重试的普通 conflict。SQLite 后端可独立先支持安全吸收；Lance 在上述引用协议全部落实后支持同一 guarded 语义。持久层安全与检索缓存刷新分别验证；schema v8 的有界变化日志覆盖独立 SQLite 句柄的查询前刷新。

## SDK 响应契约：仅作兼容增补

服务器与 SDK 应区分模型 verdict 与实际提交结果。当前 HTTP 成功计数只统计已提交的吸收/失效；冲突不计成功或错误，保留 pending 并记录 debug 日志，供下一轮重取快照。SDK 对齐既有 `by_namespace`、`absorptions`、`invalidations` 和 `purged` 字段，不新增裁决状态枚举。不能把“LLM 建议 UPDATE”计为成功 UPDATE，也不能把缺向量、零行命中或 unsupported 混入成功计数。

JSON 中新增响应字段时，旧 Serde 接收方默认忽略未知字段，使用 `deny_unknown_fields` 的客户端例外。新 SDK 读取旧服务器响应时，对新增的计数、列表等字段加 `#[serde(default)]`；新增可选详情缺失应表示未知/未提供，不能默认成 applied。[container attributes](https://serde.rs/container-attrs.html)、[field attributes](https://serde.rs/field-attrs.html)、[default](https://serde.rs/attr-default.html)

保留既有字段、类型与 enum wire 名称；新增 enum 值不会因为给外层字段加 default 就自动兼容旧解析器。JSON wire 兼容也不等于 Rust 源码兼容：向公开 struct 添加字段可能影响下游 struct literal；不应把以上结论推广到 bincode 等非自描述格式。

## 验证矩阵与未决边界

对应回归位于 [`server/src/tests.rs`](../../crates/server/src/tests.rs)、[`storage_tests.rs`](../../crates/storage/tests/storage_tests.rs)、[`lancedb_tests.rs`](../../crates/storage/tests/lancedb_tests.rs) 和 [`client/src/lib.rs`](../../crates/client/src/lib.rs)。矩阵覆盖：

- 同一 SQLite 文件的两个独立存储句柄：裁决取快照后并发编辑/删除/失效 source 或 target，旧裁决无副作用；不依赖共享 mutex。
- SQLite 与 LanceDB 调度回归：自身 DELETE 等待期间修改、改后恢复、删除、失效或归档第二候选，来源保持有效并保留 pending；候选未变化时可正常失效，删除指定候选不受无关候选更新影响。
- 吸收第二步失败时回滚第一步；success 只在 commit 成功后出现。首条 verdict 改写共享 target 后，第二条原始 verdict 冲突。
- abstract/importance 的正常后台更新不误伤 content revision；用户编辑与 mark-complete 的竞争仍被拦截。
- Lance 两实例下普通更新追加并发布新 key；搜索与 embedding 读取只消费当前 SQL 引用，吸收复制共享引用，purge source 不破坏 target。
- 注入 SQL 发布失败，确认已暂存而未发布的 key 不被检索；逐步补取跨过这些 key。追加后的进程退出、取消窗口和对象存储故障未做故障注入，孤儿允许暂存。
- SDK 分别经 HTTP 解析旧响应和新响应，验证新增 namespace map 与成功计数，以及缺失字段的缺省值。

实现对旧版 NULL 引用在吸收前检查实际向量；合法无向量记忆仍跳过向量裁决。引用回收检查所有剩余 SQL 引用，并通过 staging ledger 与 ID 扫描发现孤儿；新鲜 pending 暂不回收，过期后先撤销发布资格再删除，collecting 失败可重试。完整协议见[向量维护协议](vector-maintenance.md)。

未决项：未审计全部存储类型的底层持久化原语、Lance 冲突矩阵和 Cloud 行为；未证明任意请求取消的结果可被客户端判定；尚未证明全局搜索共同快照和完整 ANN 召回。它们不推翻 SQL 守卫与不可变引用的局部方案，但不能在实现中被默认为已有保证。源码与回归覆盖了上述局部协议；不能据此承诺跨库事务、全局召回完整性或未经故障注入的持久化保证。

## 本地验证

schema v8 的最终检查见[向量维护协议](vector-maintenance.md#本地验证)。回归覆盖双方版本冲突、edit-and-restore、失败吸收回滚、共享向量保留、跨句柄索引刷新、孤儿回收及 SDK 新旧响应。未调用外部模型或测量 benchmark 分数。
