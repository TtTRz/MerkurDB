# Web 观测台（只读）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 merkur-server 提供零依赖内嵌的只读 Web 观测台：Dashboard、记忆浏览检索、图邻域、巩固日志四视图。

**Architecture:** 3 个静态文件（index.html/app.js/style.css）以 `include_str!` 编进二进制，axum 公开路由直出；UI 为 vanilla JS hash-router SPA，经 Bearer token 调既有 `/v1/*` API + 新增 `GET /v1/memories` 浏览端点。全部数据读路径，无任何 mutation。

**Tech Stack:** Rust (axum/rusqlite/r2d2)、vanilla JS + canvas、SQLite。

**Spec:** `docs/superpowers/specs/2026-09-09-web-observability-console-design.md`

## Global Constraints

- 测试/编译用 `cargo +1.97.0`；提交前 `cargo +stable fmt` 且 `cargo +1.97.0 clippy --workspace --all-targets` 零警告。
- **commit 只在用户明确指令后执行**；message 英文、无 AI 标记、不 amend；文档与代码分开 commit。
- TDD：每个 Rust 行为先写失败测试并亲见 RED，再实现到 GREEN。
- 失效行（`invalid_at IS NOT NULL`）在列表端点永远排除——与全部检索通道语义一致。
- 不引入任何新 crate 依赖（`include_str!` 足以服务 3 个静态文件）；UI 不引框架/CDN/外部字体。
- 运行中的脚本/进程禁止编辑（bash 字节偏移坑）；长任务一律 detached（`start_new_session`）。

## 文件结构

| 文件 | 责任 |
|---|---|
| `crates/core/src/types.rs` | `MemoryListFilter` + `StorageStats.by_namespace` |
| `crates/core/src/traits.rs` | `Storage::list_memories` trait 方法 |
| `crates/storage/src/sqlite_helpers.rs` | `list_memories_filtered` 共享 SQL + `stats()` 增 namespace 分布 |
| `crates/storage/src/{sqlite,lancedb}.rs` | trait 薄封装 |
| `crates/server/src/handlers/memories.rs` | `GET /v1/memories` handler（新） |
| `crates/server/src/handlers/ui.rs` | 静态直出（新） |
| `crates/server/src/handlers/search.rs` | `parse_level_list` 改 `pub(crate)` 复用 |
| `crates/server/src/router.rs` | 注册 `/v1/memories`（protected）与 `/ui*`（public） |
| `crates/server/src/handlers/mod.rs` | 声明 `pub mod memories; pub mod ui;` |
| `crates/server/src/handlers/status.rs` | status 响应透传 `by_namespace` |
| `crates/server/ui/{index.html,app.js,style.css}` | SPA 本体 |
| `crates/server/src/tests.rs` | 端点/静态路由/状态字段测试 |
| `crates/storage/tests/storage_tests.rs` | list/stats 测试 |

---

### Task 1: Storage——`MemoryListFilter` + `list_memories`（双后端）

**Files:**
- Modify: `crates/core/src/types.rs`（追加）
- Modify: `crates/core/src/traits.rs`
- Modify: `crates/storage/src/sqlite_helpers.rs`
- Modify: `crates/storage/src/sqlite.rs`、`crates/storage/src/lancedb.rs`
- Test: `crates/storage/tests/storage_tests.rs`

**Interfaces:**
- Consumes: 现有 `Memory`、`MemoryLevel`、`Storage` trait、`sqlite_helpers` 连接池模式。
- Produces:
  - `merkur_core::MemoryListFilter { namespace: Option<String>, levels: Option<Vec<MemoryLevel>>, category: Option<String>, offset: usize, limit: usize }`（`#[derive(Debug, Clone, Default)]`）
  - `Storage::list_memories(&self, filter: &MemoryListFilter) -> MerkurResult<(Vec<Memory>, usize)>`（items, total）
  - `sqlite_helpers::list_memories_filtered(pool, filter) -> MerkurResult<(Vec<Memory>, usize)>`

- [ ] **Step 1: 写失败测试**（追加到 `crates/storage/tests/storage_tests.rs`）

```rust
#[tokio::test]
async fn test_list_memories_paginates_stably_and_excludes_invalidated() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let mut ids = Vec::new();
    for i in 0..5 {
        let mut emb = vec![0.0f32; 4];
        emb[i % 4] = 1.0;
        ids.push(
            storage
                .insert_memory(&new_test_memory(&format!("row {i}"), Some(emb)))
                .await?,
        );
        // Distinct created_at for deterministic DESC order.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // One row leaves the visible set.
    storage.invalidate_memory(&ids[4], None).await?;

    let page1 = storage
        .list_memories(&merkur_core::MemoryListFilter {
            limit: 2,
            offset: 0,
            ..Default::default()
        })
        .await?;
    let page2 = storage
        .list_memories(&merkur_core::MemoryListFilter {
            limit: 2,
            offset: 2,
            ..Default::default()
        })
        .await?;
    let page3 = storage
        .list_memories(&merkur_core::MemoryListFilter {
            limit: 2,
            offset: 4,
            ..Default::default()
        })
        .await?;

    assert_eq!(page1.1, 4, "total excludes the invalidated row");
    assert_eq!(page1.0.len(), 2);
    assert_eq!(page2.0.len(), 2);
    assert!(page3.0.is_empty(), "offset past the visible set is empty, not wrapped");
    let seen: std::collections::HashSet<&str> = page1
        .0
        .iter()
        .chain(&page2.0)
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(seen.len(), 4, "pages must not repeat rows");
    assert!(!seen.contains(ids[4].as_str()), "invalidated row must not appear");
    // created_at DESC: newest first.
    assert!(page1.0[0].created_at >= page1.0[1].created_at);
    Ok(())
}

#[tokio::test]
async fn test_list_memories_filters_level_category_namespace() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let a = storage
        .insert_memory(&new_test_memory("alpha fact", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let mut foreign = new_test_memory("beta fact", Some(vec![0.0, 1.0, 0.0, 0.0]));
    foreign.namespace = "beta".into();
    let _b = storage.insert_memory(&foreign).await?;

    let (default_items, default_total) = storage
        .list_memories(&merkur_core::MemoryListFilter {
            namespace: Some(merkur_core::DEFAULT_NAMESPACE.to_string()),
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(default_total, 1, "beta bucket must not leak into default");
    assert_eq!(default_items[0].id, a);

    let (cat_items, _) = storage
        .list_memories(&merkur_core::MemoryListFilter {
            category: Some("general".into()),
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert!(cat_items.iter().any(|m| m.id == a), "category filter keeps matches");

    let (none, zero) = storage
        .list_memories(&merkur_core::MemoryListFilter {
            category: Some("nonexistent".into()),
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert!(none.is_empty() && zero == 0);
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认 RED**

Run: `cargo +1.97.0 test -p merkur-storage list_memories 2>&1 | tail -5`
Expected: 编译失败 `no function or associated item named 'list_memories'`

- [ ] **Step 3: 实现**

`crates/core/src/types.rs` 追加：

```rust
#[derive(Debug, Clone, Default)]
pub struct MemoryListFilter {
    pub namespace: Option<String>,
    pub levels: Option<Vec<MemoryLevel>>,
    pub category: Option<String>,
    pub offset: usize,
    pub limit: usize,
}
```

`crates/core/src/traits.rs` 在 `Storage` trait 中（放在 `get_memory` 附近）：

```rust
    /// Browse listing for the observability console: filter + paginate the
    /// live store. Always excludes soft-invalidated rows, like every
    /// retrieval channel.
    async fn list_memories(
        &self,
        filter: &crate::MemoryListFilter,
    ) -> MerkurResult<(Vec<crate::Memory>, usize)>;
```

`crates/storage/src/sqlite_helpers.rs` 追加（投影列与 `get_memory_row` 的 SELECT 保持一致——若该文件已有行→Memory 的映射闭包则提取复用，否则按同款列序映射）：

```rust
pub fn list_memories_filtered(
    pool: &Pool<SqliteConnectionManager>,
    filter: &merkur_core::MemoryListFilter,
) -> MerkurResult<(Vec<Memory>, usize)> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let levels_json = filter
        .levels
        .as_ref()
        .map(|ls| serde_json::to_string(&ls.iter().map(|l| l.as_i32()).collect::<Vec<_>>()).unwrap());
    let where_sql = "invalid_at IS NULL
        AND (?1 IS NULL OR namespace = ?1)
        AND (?2 IS NULL OR level IN (SELECT value FROM json_each(?2)))
        AND (?3 IS NULL OR category = ?3)";
    let total: usize = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM memories WHERE {where_sql}"),
            params![filter.namespace, levels_json, filter.category],
            |row| row.get(0),
        )
        .map_err(|e| MerkurError::Storage(format!("list count failed: {e}")))?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id, content, abstract, category, weight, level, pending_consolidation,
                    metadata, created_at, updated_at, accessed_at, access_count, namespace,
                    importance, valid_at, invalid_at
             FROM memories WHERE {where_sql}
             ORDER BY created_at DESC, id ASC
             LIMIT ?4 OFFSET ?5"
        ))
        .map_err(|e| MerkurError::Storage(format!("list prepare failed: {e}")))?;
    let rows = stmt
        .query_map(
            params![filter.namespace, levels_json, filter.category, filter.limit as i64, filter.offset as i64],
            memory_row_mapper,
        )
        .map_err(|e| MerkurError::Storage(format!("list query failed: {e}")))?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| MerkurError::Storage(format!("list row failed: {e}")))?);
    }
    Ok((items, total))
}
```

注意：`memory_row_mapper` 需与 `get_memory_row` 用同一映射；实现时先读该函数现状，若其映射是内联闭包则提取为 `fn memory_row_mapper(row: &rusqlite::Row) -> rusqlite::Result<Memory>` 共用（顺带小幅去重，属本任务范围）。`MemoryLevel::as_i32()` 若不存在则用既有 `from_i32` 的逆向（读代码确认真实命名）。

`crates/storage/src/sqlite.rs` 与 `lancedb.rs` 各自实现（照搬 `bfs_expand_ns` 的 run_blocking 模式）：

```rust
    async fn list_memories(
        &self,
        filter: &MemoryListFilter,
    ) -> MerkurResult<(Vec<Memory>, usize)> {
        let filter = filter.clone();
        let pool = self.pool.clone(); // lancedb: self.sqlite_pool
        run_blocking(move || sqlite_helpers::list_memories_filtered(&pool, &filter)).await
    }
```

- [ ] **Step 4: 跑测试确认 GREEN**

Run: `cargo +1.97.0 test -p merkur-storage list_memories 2>&1 | tail -3`
Expected: 2 passed

- [ ] **Step 5: 全量回归 + clippy + fmt**

Run: `cargo +1.97.0 test --workspace 2>&1 | grep -c "test result: ok"` 应等于基线套件数；`cargo +1.97.0 clippy --workspace --all-targets` 零警告；`cargo +stable fmt`。

---

### Task 2: `StorageStats.by_namespace`

**Files:**
- Modify: `crates/core/src/types.rs`（StorageStats）
- Modify: `crates/storage/src/sqlite_helpers.rs`（stats fn）
- Test: `crates/storage/tests/storage_tests.rs`

**Interfaces:**
- Consumes: Task 1 无依赖（独立小改）。
- Produces: `StorageStats { total_memories, total_edges, pending_consolidation, by_level: HashMap<i32, usize>, by_namespace: HashMap<String, usize> }`——Task 3 的 status handler 与 Task 7 的客户端透传字段名 `by_namespace`。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn test_stats_groups_by_namespace() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    storage
        .insert_memory(&new_test_memory("one", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let mut foreign = new_test_memory("two", Some(vec![0.0, 1.0, 0.0, 0.0]));
    foreign.namespace = "team-x".into();
    storage.insert_memory(&foreign).await?;

    let stats = storage.stats().await?;
    assert_eq!(
        stats.by_namespace.get(merkur_core::DEFAULT_NAMESPACE),
        Some(&1)
    );
    assert_eq!(stats.by_namespace.get("team-x"), Some(&1));
    Ok(())
}
```

- [ ] **Step 2: RED** — `cargo +1.97.0 test -p merkur-storage test_stats_groups_by_namespace 2>&1 | tail -4`，预期编译失败 `no field 'by_namespace'`

- [ ] **Step 3: 实现** — `types.rs` 的 `StorageStats` 加 `pub by_namespace: HashMap<String, usize>`；`sqlite_helpers.rs` 的 `stats()` 在 `by_level` 查询后追加同模式查询：`SELECT namespace, COUNT(*) FROM memories GROUP BY namespace` 装入 `by_namespace`。修复所有 `StorageStats` 字面量构造点（编译器会指出，预期 core mock/storage 各一处）。

- [ ] **Step 4: GREEN + 回归** — 同 Task 1 Step 4/5。

---

### Task 3: `GET /v1/memories` 端点

**Files:**
- Create: `crates/server/src/handlers/memories.rs`
- Modify: `crates/server/src/handlers/mod.rs`、`crates/server/src/router.rs`、`crates/server/src/handlers/search.rs`（`parse_level_list` → `pub(crate)`）、`crates/server/src/handlers/status.rs`
- Test: `crates/server/src/tests.rs`

**Interfaces:**
- Consumes: Task 1 的 `Storage::list_memories` / `MemoryListFilter`；Task 2 的 `stats.by_namespace`；现有 `Namespace` extractor、`parse_level_list`（search.rs 私有 fn，本任务改 `pub(crate)`）。
- Produces:
  - `GET /v1/memories?namespace&level&category&offset&limit` → `{"items": [...], "total": N, "offset": N, "limit": N}`；items 字段 = `id/content/abstract/weight/level/category/context/created_at/namespace/importance/invalid_at`（与 search 结果同构、无 score、无 embedding）
  - `GET /v1/status` 响应增 `by_namespace` 字段

- [ ] **Step 1: 写失败测试**（追加到 `crates/server/src/tests.rs`，紧跟现有 search 测试风格）

```rust
    #[tokio::test]
    async fn test_list_memories_endpoint_filters_paginates_and_isolates() {
        let state = test_app().await;
        let app = router::create_router(state);

        // 3 default + 1 foreign-namespace memories.
        for (content, ns) in [
            ("alpha one", None),
            ("alpha two", None),
            ("alpha three", None),
            ("beta fact", Some("beta")),
        ] {
            let mut req = Request::post("/v1/write")
                .header("content-type", "application/json");
            if let Some(ns) = ns {
                req = req.header("x-merkur-namespace", ns);
            }
            let resp = app
                .clone()
                .oneshot(req.body(Body::from(format!(r#"{{"content":"{content}"}}"#))).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let get = |uri: &str| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()
            }
        };

        let page1 = get("/v1/memories?limit=2&offset=0").await;
        assert_eq!(page1["total"], 3);
        assert_eq!(page1["items"].as_array().unwrap().len(), 2);
        let page2 = get("/v1/memories?limit=2&offset=2").await;
        assert_eq!(page2["items"].as_array().unwrap().len(), 1);
        let ids: std::collections::HashSet<&str> = page1["items"]
            .as_array()
            .unwrap()
            .iter()
            .chain(page2["items"].as_array().unwrap())
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(ids.len(), 3, "pages must not repeat and must not leak beta");
        assert!(
            page1["items"][0]["embedding"].is_null() || page1["items"][0].get("embedding").is_none(),
            "embeddings never leave the API"
        );

        let beta = get("/v1/memories?namespace=beta").await;
        assert_eq!(beta["total"], 1);
        assert_eq!(beta["items"][0]["content"], "beta fact");

        let status = get("/v1/status").await;
        assert_eq!(status["by_namespace"]["default"], 3);
        assert_eq!(status["by_namespace"]["beta"], 1);
    }
```

- [ ] **Step 2: RED** — `cargo +1.97.0 test -p merkur-server test_list_memories_endpoint 2>&1 | tail -4`，预期 404 或编译失败。

- [ ] **Step 3: 实现**

`crates/server/src/handlers/memories.rs`：

```rust
use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use merkur_core::MemoryListFilter;
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::ApiResult;
use crate::handlers::namespace::Namespace;
use crate::handlers::search::parse_level_list;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub namespace: Option<String>,
    pub level: Option<String>,
    pub category: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub async fn list_memories(
    State(state): State<AppState>,
    ns: Namespace,
    Query(params): Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    let filter = MemoryListFilter {
        namespace: Some(params.namespace.unwrap_or(ns.0)),
        levels: params.level.as_deref().map(parse_level_list),
        category: params.category,
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(20).clamp(1, 200),
    };
    let (items, total) = state.storage.list_memories(&filter).await?;
    Ok(Json(json!({
        "items": items.iter().map(|m| json!({
            "id": m.id,
            "content": m.content,
            "abstract": m.abstract_,
            "weight": m.weight,
            "level": m.level,
            "category": m.category,
            "context": m.context,
            "created_at": m.created_at,
            "namespace": m.namespace,
            "importance": m.importance,
            "invalid_at": m.invalid_at,
        })).collect::<Vec<_>>(),
        "total": total,
        "offset": filter.offset,
        "limit": filter.limit,
    })))
}
```

`search.rs` 中 `fn parse_level_list` 改为 `pub(crate) fn parse_level_list`；`handlers/mod.rs` 加 `pub mod memories;`；`router.rs` 的 protected 组加 `.route("/v1/memories", get(handlers::memories::list_memories))`；`status.rs` 的 json! 加 `"by_namespace": stats.by_namespace`。

- [ ] **Step 4: GREEN + 回归** — `cargo +1.97.0 test -p merkur-server` 全绿；workspace 回归 + clippy + fmt（同 Task 1 Step 5）。

---

### Task 4: 静态路由 `/ui`（壳 + 占位文件）

**Files:**
- Create: `crates/server/src/handlers/ui.rs`
- Create: `crates/server/ui/index.html`、`crates/server/ui/app.js`、`crates/server/ui/style.css`（本任务只放可编译占位；Task 5/6 填实）
- Modify: `crates/server/src/handlers/mod.rs`、`crates/server/src/router.rs`
- Test: `crates/server/src/tests.rs`

**Interfaces:**
- Produces: `GET /ui`（text/html）、`GET /ui/app.js`（text/javascript）、`GET /ui/style.css`（text/css），全部挂在 **public** 路由组（与 /v1/health 同组，无 auth）。静态文件物理路径 `crates/server/ui/`。

- [ ] **Step 1: 写失败测试**

```rust
    #[tokio::test]
    async fn test_ui_static_routes() {
        let state = test_app().await;
        let app = router::create_router(state);
        for (uri, ct) in [
            ("/ui", "text/html"),
            ("/ui/app.js", "text/javascript"),
            ("/ui/style.css", "text/css"),
        ] {
            let resp = app
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let got = resp
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(got.starts_with(ct), "{uri} content-type: {got}");
        }
    }
```

- [ ] **Step 2: RED** — 404。

- [ ] **Step 3: 实现**

`crates/server/src/handlers/ui.rs`：

```rust
use axum::response::IntoResponse;

pub async fn index() -> impl IntoResponse {
    (
        [("content-type", "text/html; charset=utf-8")],
        include_str!("../../ui/index.html"),
    )
}

pub async fn app_js() -> impl IntoResponse {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../../ui/app.js"),
    )
}

pub async fn style_css() -> impl IntoResponse {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("../../ui/style.css"),
    )
}
```

占位文件：`index.html` 放 `<!doctype html><title>MerkurDB</title><div id="app"></div><script src="/ui/app.js"></script>`；`app.js`、`style.css` 各放一行注释。`handlers/mod.rs` 加 `pub mod ui;`；`router.rs` public 组加：

```rust
        .route("/ui", get(handlers::ui::index))
        .route("/ui/app.js", get(handlers::ui::app_js))
        .route("/ui/style.css", get(handlers::ui::style_css))
```

- [ ] **Step 4: GREEN + 回归**（同前）。

---

### Task 5: UI——壳 + Dashboard + 记忆浏览/详情

**Files:**
- Modify: `crates/server/ui/index.html`、`crates/server/ui/app.js`、`crates/server/ui/style.css`

**Interfaces:**
- Consumes: Task 3 的 `/v1/memories`、`/v1/status`（含 by_namespace）、既有 `GET /v1/memory/{id}`、`GET /v1/graph/{id}`。
- Produces: hash 路由 `#/dashboard`、`#/memories`、`#/memories/:id`；`api()` fetch 封装（后续 Task 6 复用）。

- [ ] **Step 1: `index.html`**——顶栏（产品名 + token 输入框 + 保存按钮 + 导航链接）+ `<main id="view">`。导航：`#/dashboard` 总览 · `#/memories` 记忆 · `#/log` 日志。

- [ ] **Step 2: `app.js` 基础设施**

```js
const store = {
  get token() { return localStorage.getItem('merkur_token') || '' },
  set token(v) { v ? localStorage.setItem('merkur_token', v) : localStorage.removeItem('merkur_token') },
};

async function api(path) {
  const resp = await fetch(path, { headers: { Authorization: `Bearer ${store.token}` } });
  if (resp.status === 401) { store.token = ''; showTokenGate(); throw new Error('401'); }
  if (!resp.ok) throw new Error(`${resp.status} ${await resp.text()}`);
  return resp.json();
}

const routes = {
  '/dashboard': renderDashboard,
  '/memories': renderMemories,
  '/log': renderLog,          // Task 6
};
function currentRoute() {
  const h = location.hash.slice(1) || '/dashboard';
  const m = h.match(/^\/memory\/(.+)$/);   if (m) return ['memory', m[1]];
  const g = h.match(/^\/graph\/(.+)$/);    if (g) return ['graph', g[1]];
  return ['static', routes[h] ? h : '/dashboard'];
}
window.addEventListener('hashchange', render);
```

- [ ] **Step 3: Dashboard** — `api('/v1/status')` 渲染卡片行（total_memories / total_edges / pending_consolidation / uptime 人性化格式）+ 两张分布表（by_level：level 名映射 `{"-1":"archived","0":"title","1":"summary","2":"full"}`；by_namespace：表格 + 点击跳 `#/memories?ns=<name>`）。

- [ ] **Step 4: 记忆浏览** — 过滤栏（namespace 文本、level 下拉（all/full/summary/title/archived）、category 文本、应用按钮）+ 表格（content 前 120 字符截断、level 徽章、importance、weight、created_at 本地时区）+ 分页条（prev/next + `第 offset/limit 页 · 共 total 条`）；行点击跳 `#/memory/:id`。查询参数从 hash 解析（`#/memories?ns=x`）。

- [ ] **Step 5: 记忆详情** — `api('/v1/memory/{id}')` 全字段渲染（content 完整、abstract、importance/weight/access_count、created/updated/accessed、context 表、**invalid_at 非空时顶部红色失效横幅**）+ `api('/v1/graph/{id}')` 的 edges 列表（source→relation(weight)→target，可点击跳对端详情）+ “图视图打开”按钮跳 `#/graph/:id`。

- [ ] **Step 6: 浏览器手验**——`MERKUR_TOKEN=t cargo +1.97.0 run --release -p merkur-server --features openai -- --config config.example.yaml`（或现有测试 db 配置），浏览器开 `http://localhost:1934/ui`：token 保存 → 四导航可切 → 过滤/分页工作 → 详情字段全 → 401 清空重问。

---

### Task 6: UI——图邻域 + 巩固日志

**Files:**
- Modify: `crates/server/ui/app.js`、`crates/server/ui/style.css`

**Interfaces:**
- Consumes: Task 5 的 `api()`/路由骨架；`GET /v1/graph/{id}?depth=2`（`{center, neighborhood:[{id,content,abstract,score,level}], edges:[{source_id,target_id,weight,relation,edge_type}]}`）；`GET /v1/consolidate/log?limit=100`（`{entries:[...]}`——字段以实际响应为准，实现时先 curl 一次真实服务确认 key 名再写渲染）。
- Produces: `#/graph/:id` 与 `#/log` 视图。

- [ ] **Step 1: 图视图** — canvas 力导向（无库）：

```js
function forceLayout(nodes, edges, iterations = 300) {
  const pos = new Map(nodes.map((n, i) => [n.id, {
    x: 400 + 250 * Math.cos(i * 2.399), y: 300 + 250 * Math.sin(i * 2.399), vx: 0, vy: 0,
  }]));
  for (let k = 0; k < iterations; k++) {
    for (const [id, p] of pos) {                     // 电荷斥力
      for (const [id2, q] of pos) {
        if (id === id2) continue;
        let dx = p.x - q.x, dy = p.y - q.y, d2 = dx * dx + dy * dy + 1;
        const f = Math.min(4000 / d2, 4);
        p.vx += dx * f / Math.sqrt(d2) * 0.5; p.vy += dy * f / Math.sqrt(d2) * 0.5;
      }
    }
    for (const e of edges) {                         // 弹簧引力
      const p = pos.get(e.source_id), q = pos.get(e.target_id);
      if (!p || !q) continue;
      let dx = q.x - p.x, dy = q.y - p.y;
      p.vx += dx * 0.005 * e.weight; p.vy += dy * 0.005 * e.weight;
      q.vx -= dx * 0.005 * e.weight; q.vy -= dy * 0.005 * e.weight;
    }
    for (const p of pos.values()) { p.vx *= 0.85; p.vy *= 0.85; p.x += p.vx; p.y += p.vy; }
  }
  return pos;
}
```

渲染：边（灰线，weight 映射线宽），节点（圆，center 高亮色、按 level 描边），点击节点跳 `#/memory/:id`（canvas click → 最近节点命中检测），标题栏显示 center id + 节点/边数。空邻域画占位文案。

- [ ] **Step 2: 日志视图** — `api('/v1/consolidate/log?limit=100')` 表格：时间（本地）、processed / edges_created / absorptions / invalidations / errors（以对真实服务 curl 得到的字段名为准）；errors > 0 行标红。

- [ ] **Step 3: style.css 补图视图/日志样式**——canvas 全宽容器 600px 高、表格斑马纹、徽章、失效红。

- [ ] **Step 4: 浏览器手验**——图渲染不重叠严重、点击跳详情、日志表数据正确。

---

### Task 7: 文档——openapi + README + CHANGELOG

**Files:**
- Modify: `openapi.yaml`、`README.md`、`README_CN.md`、`CHANGELOG.md`

**Interfaces:**
- Consumes: Task 3 的端点契约、Task 2 的字段名 `by_namespace`。

- [ ] **Step 1: openapi.yaml** — paths 加 `/v1/memories`（get；参数 namespace/level/category/offset/limit 的 schema 与 `/v1/search` 同款；响应 items+total+offset+limit）；`/v1/status` 响应 schema 加 `by_namespace: {type: object, additionalProperties: {type: integer}}`；`/ui` 三条静态路由记为 public 信息端点。

- [ ] **Step 2: README ×2** — Key Features / 核心特性 各加一行：只读 Web 观测台内嵌于服务（`/ui`：状态总览、记忆浏览、图邻域、巩固日志）；Quick Start 段加一行打开方式。

- [ ] **Step 3: CHANGELOG** — Unreleased Added 加：`GET /v1/memories` 浏览端点（过滤/分页/失效排除）+ `/v1/status` 增 `by_namespace` + 内嵌只读 Web 观测台（`/ui`）。

---

### Task 8: 端到端验证 + 交付

**Files:** 无（纯验证）

- [ ] **Step 1:** 起服务（真实 db，可复用 `crates/eval/data/tune/merkur.db` 的 sweep 配置或现造小数据）+ curl 冒烟：`/v1/memories` 分页/total、`/v1/status` by_namespace、`/ui` 200。
- [ ] **Step 2:** 浏览器完整走查：token 流程 → 四视图 → 过滤/分页 → 详情失效横幅（预先用客户端 invalidate 一条）→ 图渲染/点击 → 日志表。
- [ ] **Step 3:** `cargo +1.97.0 test --workspace` 全绿 + clippy 零警告 + `cargo +stable fmt --check` 干净。
- [ ] **Step 4:** 向用户汇报并等 commit 指令（代码与文档分两个 commit；spec/plan 文档随行）。

## 自检记录

- **Spec 覆盖**：spec §3.1→Task 4；§3.2→Task 1/3；§3.3→Task 2/3；§4 视图→Task 5/6；§5 错误处理→Task 5（401 流程）/3（clamp）；§6 测试→各 Task 测试步 + Task 8；§7 交付物→文件结构表。无缺口。
- **类型一致性**：`MemoryListFilter` 字段名 Task 1=Task 3；`by_namespace` Task 2=Task 3=Task 7；`parse_level_list` 复用同一 fn；静态路由三件套路径一致（`/ui`、`/ui/app.js`、`/ui/style.css` ↔ `crates/server/ui/`）。
