# MerkurDB Web 观测台（只读）设计

> 日期：2026-09-09 · 状态：已批准（用户口头 OK）
> 范围反转说明：原 roadmap 将 Web Dashboard 列为"不做"，经用户指示正式立项，边界收窄为**只读观测台**。

## 1. 目标与边界

为 MerkurDB 提供一个零运维负担的只读观测台：打开浏览器即可看清记忆库状态、浏览检索记忆、探索图邻域、审计巩固日志。

**做**：Dashboard 总览、记忆浏览检索、图邻域视图、巩固日志。
**不做**：任何写路径（write/update/delete/trigger）、登录系统、前端框架、Node 工具链、独立部署形态。

## 2. 架构

单一二进制身份不变：3 个静态文件（index.html / app.js / style.css）以 `include_str!` 编译进 `merkur-server`，由 axum 静态路由直出。UI 是 vanilla JS SPA（hash 路由 + fetch），无构建步骤。

```
browser ──GET /ui{,/app.js,/style.css}──► axum 静态路由（公开，无 auth）
browser ──fetch /v1/* (Bearer token)────► 现有受保护 API + 新 /v1/memories
token 首次输入后存 localStorage；401 时 UI 清空重问
```

## 3. 服务端改动

### 3.1 静态路由（`crates/server/src/handlers/ui.rs` + router 注册）

- `GET /ui` → text/html（index.html）
- `GET /ui/app.js` → text/javascript
- `GET /ui/style.css` → text/css
- 挂在 public 路由组（与 /v1/health 同组）：静态资源不含敏感信息；数据仍全部走 token 门控的 /v1/*。

### 3.2 新端点 `GET /v1/memories`

浏览态需要无 query 的列表（现有 `/v1/search` 强制 `q`）。

- 参数：`namespace`（默认 default）、`level`（逗号列表，同 search）、`category`、`offset`（默认 0）、`limit`（默认 20，clamp 1..200）
- 响应：`{ items: [...], total, offset, limit }`——items 与 `/v1/search` 结果同构（id / content / abstract / weight / level / category / context / created_at / namespace / importance / invalid_at），只是没有 score；不返回 embedding 向量本体
- 排序：`created_at DESC, id`（稳定分页）
- 语义：永远排除 `invalid_at IS NOT NULL`（观测台看活库；失效记忆经详情页可审计）——与所有检索通道一致
- 实现：`sqlite_helpers::list_memories_filtered()` 共享 SQL，两个后端各自的 `list_memories()` trait 方法薄封装（`LanceDbStorage` 骑同一 SQLite）。`Storage` trait 增加 `list_memories(filter) -> (Vec<Memory>, usize)`

### 3.3 stats 增 `by_namespace`

`sqlite_helpers::stats()` 增加 `SELECT namespace, COUNT(*) GROUP BY namespace`；`Stats` 结构与 `/v1/status` 响应增 `by_namespace: HashMap<String, usize>`（与既有 `by_level` 同模式，向后兼容——客户端忽略未知字段）。

## 4. UI 结构（vanilla，~700 行）

- `index.html`：骨架 + 顶栏（token 输入、导航）+ 视图容器
- `app.js`：
  - `api(path)` fetch 封装（Bearer 注入、401→清空 token 重问）
  - hash 路由：`#/dashboard`、`#/memories`、`#/memories/:id`、`#/graph/:id`、`#/log`
  - 视图函数 ×4，各自负责 fetch + 渲染到容器
  - 图视图：canvas 力导向布局（~150 行：弹簧-电荷迭代，无库）
- `style.css`：深色主题，紧凑表格，失效态标红，level 徽章
- 无框架、无 CDN、无外部字体——完全离线可用

### 视图数据映射

| 视图 | 数据源 |
|---|---|
| Dashboard | `GET /v1/status`（totals, pending, by_level, by_namespace, uptime） |
| 记忆浏览 | `GET /v1/memories`（过滤栏：namespace/level/category；分页） |
| 记忆详情 | `GET /v1/memory/{id}` + `GET /v1/graph/{id}`（边列表） |
| 图邻域 | `GET /v1/graph/{id}?depth=2` |
| 巩固日志 | `GET /v1/consolidate/log?limit=100` |

## 5. 错误处理

- 401 → UI 清 token 重问；429 → 显示限流提示并重试按钮
- 新端点非法参数（level 未知值）与 search 同策略：静默跳过未知 token
- 列表空态 / 图空邻域 / 日志空态各自有占位文案
- 服务端：`/v1/memories` 的 limit 超界 clamp（不报错）

## 6. 测试

- 服务端 TDD：
  - `/v1/memories` 分页稳定（同页不重复）、过滤（level/category）、namespace 隔离（他桶不可见）、失效行排除、total 与分页一致性
  - stats `by_namespace` 分布正确
  - 静态路由 200 + content-type
- UI：无测试基建（vanilla 形态决定）；交付前浏览器实跑验证（golden path：四视图 + 过滤 + 分页 + 图渲染 + 401 流程）
- `openapi.yaml` 补 `/v1/memories` 与 `by_namespace`

## 7. 交付物清单

| 层 | 文件 |
|---|---|
| server | `handlers/ui.rs`（新）、`handlers/memories.rs`（新）、`router.rs`、`traits.rs`、`sqlite.rs`、`lancedb.rs`、`sqlite_helpers.rs`、`types.rs`（Stats） |
| tests | `server/src/tests.rs`、`storage/tests/storage_tests.rs` |
| UI | `crates/server/ui/index.html`、`app.js`、`style.css` |
| 文档 | `openapi.yaml`、`README.md`、`README_CN.md`、`CHANGELOG.md` |
