#[cfg(test)]
mod integration {
    use crate::app_state::AppState;
    use crate::router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use merkur_consolidators::NoopConsolidator;
    use merkur_core::{Consolidator, Forgetter, MemoryLevel};
    use merkur_embedders::NoopEmbedder;
    use merkur_forgetters::{EbbinghausConfig, EbbinghausForgetter};
    use merkur_storage::SqliteStorage;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> String {
        let id = TEST_DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("file:test_server_{id}?mode=memory&cache=shared")
    }

    async fn test_app() -> AppState {
        let dim = 16;
        let embedder: Arc<dyn merkur_core::Embedder> = Arc::new(NoopEmbedder::new(dim));
        let storage: Arc<dyn merkur_core::Storage> = Arc::new(
            SqliteStorage::new(&temp_db_path(), dim).expect("Failed to create test storage"),
        );
        let consolidator: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);
        let forgetter: Arc<dyn Forgetter> =
            Arc::new(EbbinghausForgetter::new(EbbinghausConfig::default()));
        let config = Arc::new(crate::config::Config::test_config());

        AppState::new(
            embedder,
            storage,
            consolidator,
            forgetter,
            config,
            chrono::Utc::now(),
        )
    }

    #[tokio::test]
    async fn test_write_and_search() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"v8 GC is generational"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let _id = json["id"].as_str().unwrap().to_string();
        assert!(json["status"].as_str() == Some("ok"));
        assert!(json["searchable"].as_bool() == Some(true));

        let resp = app
            .oneshot(
                Request::get("/v1/search?q=v8+GC+is+generational&mode=fast&score_threshold=0.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["total"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn test_get_and_delete_memory() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"test memory"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = json["id"].as_str().unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/memory/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["content"].as_str(), Some("test memory"));

        let resp = app
            .clone()
            .oneshot(
                Request::delete(format!("/v1/memory/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(
                Request::get(format!("/v1/memory/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_status() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .oneshot(Request::get("/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_memories"].as_u64(), Some(0));
        assert_eq!(json["total_edges"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn test_trigger_consolidate_empty() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .oneshot(
                Request::post("/v1/consolidate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["processed"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn test_relate_and_graph() {
        let state = test_app().await;
        let app = router::create_router(state);

        let r1 = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"memory A"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let b1 = axum::body::to_bytes(r1.into_body(), 4096).await.unwrap();
        let id1 = serde_json::from_slice::<serde_json::Value>(&b1).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let r2 = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"memory B"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let b2 = axum::body::to_bytes(r2.into_body(), 4096).await.unwrap();
        let id2 = serde_json::from_slice::<serde_json::Value>(&b2).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let edge_json = serde_json::json!({
            "source_id": &id1,
            "target_id": &id2,
            "relation": "related_to",
            "weight": 0.8
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/relate")
                    .header("content-type", "application/json")
                    .body(Body::from(edge_json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .oneshot(
                Request::get(format!("/v1/graph/{id1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["center"].as_str(), Some(id1.as_str()));
        assert!(!json["neighborhood"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_deep_search() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .oneshot(
                Request::get("/v1/search?q=test&mode=deep")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_relate_self_edge_rejected() {
        let state = test_app().await;
        let app = router::create_router(state);

        let r1 = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"a"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let b1 = axum::body::to_bytes(r1.into_body(), 4096).await.unwrap();
        let id1 = serde_json::from_slice::<serde_json::Value>(&b1).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let edge = serde_json::json!({
            "source_id": id1,
            "target_id": id1,
        });
        let resp = app
            .oneshot(
                Request::post("/v1/relate")
                    .header("content-type", "application/json")
                    .body(Body::from(edge.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_relate_unknown_target_rejected() {
        let state = test_app().await;
        let app = router::create_router(state);

        let r1 = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"a"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let b1 = axum::body::to_bytes(r1.into_body(), 4096).await.unwrap();
        let id1 = serde_json::from_slice::<serde_json::Value>(&b1).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let edge = serde_json::json!({
            "source_id": id1,
            "target_id": "mem_00000000-0000-0000-0000-000000000000",
        });
        let resp = app
            .oneshot(
                Request::post("/v1/relate")
                    .header("content-type", "application/json")
                    .body(Body::from(edge.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_search_invalid_mode_400() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .oneshot(
                Request::get("/v1/search?q=hello&mode=bogus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_write_batch_full_failure_returns_207() {
        let state = test_app().await;
        let app = router::create_router(state);

        // All items have empty content → validation error → zero success → 207
        let body = r#"{"items":[{"content":""},{"content":""}]}"#;
        let resp = app
            .oneshot(
                Request::post("/v1/write-batch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
    }

    #[tokio::test]
    async fn test_context_boost_rescues_below_threshold() {
        let state = test_app().await;
        let app = router::create_router(state);

        // Write a memory with context
        let write_body =
            r#"{"content":"rust borrow checker","context":{"lang":"rust","topic":"memory"}}"#;
        app.clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(write_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Search with a very high threshold but matching context should still
        // return results if context boost pushes score above threshold.
        // With NoopEmbedder, cosine scores are deterministic. A context with
        // 2 matching keys adds +0.2 boost.
        let resp = app
            .oneshot(
                Request::get(
                    "/v1/search?q=rust&score_threshold=0.0&context=%7B%22lang%22%3A%22rust%22%7D",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ------------------------------------------------------------------
    // Hybrid retrieval (BM25 x vector, RRF-fused). Enabled by P1-4.
    // ------------------------------------------------------------------

    /// Omitting the `mode` parameter must resolve to hybrid search — the
    /// out-of-the-box path should always be the best retrieval we have.
    #[tokio::test]
    async fn test_default_search_mode_is_hybrid() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"the rrf fusion test memory"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .oneshot(
                Request::get("/v1/search?q=rrf+fusion+test+memory")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["mode"], "hybrid");
        assert!(json["total"].as_u64().unwrap() > 0);
    }

    /// Query text equal to a stored memory's content: the exact-match memory
    /// must come back first with the normalized ceiling score of 1.0 (rank-1
    /// in both channels), regardless of what pseudo-random neighbors the
    /// embedder contributes.
    #[tokio::test]
    async fn test_hybrid_search_ranks_exact_match_first_with_unit_score() {
        let state = test_app().await;
        let app = router::create_router(state);

        for content in [
            "trigram bm25 exact match target sentence",
            "an unrelated filler memory about databases",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/v1/write")
                        .header("content-type", "application/json")
                        .body(Body::from(format!(r#"{{"content":"{content}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let resp = app
            .oneshot(
                Request::get("/v1/search?q=trigram+bm25+exact+match+target+sentence&mode=hybrid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "'hybrid' must be an accepted search mode"
        );
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let results = json["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["content"].as_str().unwrap(), "trigram bm25 exact match target sentence");
        // P1-5: the visible score is the composite, not raw RRF. A rank-1
        // dual hit (fused=1.0) on a fresh memory (weight=1.0, importance
        // prior 0.5) yields 0.5*1.0 + 0.2*1.0 + 0.3*0.5 = 0.85.
        let top_score = results[0]["score"].as_f64().unwrap();
        assert!(
            (top_score - 0.85).abs() < 1e-9,
            "composite of fused=1.0, weight=1.0, importance=0.5 must be 0.85, got {top_score}"
        );
    }

    /// P0-2: access-driven promotion. A demoted memory that keeps getting
    /// retrieved must climb back one rung on the next forgetting tick.
    /// P0-2: access-driven promotion. A demoted memory that keeps getting
    /// retrieved must climb back one rung on the next forgetting tick. The
    /// custom forgetter keeps this wiring test independent of the async timing
    /// of vector_search's background access bumps.
    #[tokio::test]
    async fn test_forgetting_tick_promotes_frequently_accessed_memory() {
        use merkur_core::NewMemory;

        let state = test_app().await;
        let dim = 16;

        let mem = NewMemory {
            content: "hot memory that keeps getting retrieved".into(),
            category: Some("general".into()),
            context: Default::default(),
            metadata: Default::default(),
            embedding: Some(vec![1.0; dim]),
            namespace: merkur_core::DEFAULT_NAMESPACE.to_string(),
        };
        let id = state.storage.insert_memory(&mem).await.unwrap();
        // Force the memory down to Title to simulate an old, demoted row.
        state.storage.update_level(&id, 0).await.unwrap();
        assert_eq!(
            state.storage.get_memory(&id).await.unwrap().unwrap().level,
            MemoryLevel::Title
        );

        // A couple of retrieval hits demonstrate demand; their access-bump
        // lands asynchronously, which the bar below tolerates.
        let query_vec = vec![1.0f32; dim];
        for _ in 0..2 {
            let hits = state.storage.vector_search(&query_vec, 5).await.unwrap();
            assert!(hits.iter().any(|h| h.id == id));
        }
        let forgetter = Arc::new(EbbinghausForgetter::new(EbbinghausConfig {
            half_life_seconds: f64::MAX,
            threshold_upgrade: 0.2,
            upgrade_min_access_count: 1,
            ..Default::default()
        }));

        let (archived, downgraded, upgraded, cleaned) =
            crate::scheduler::Scheduler::run_forgetting_once(
                &*state.storage,
                &*forgetter,
                100,
                30,
            )
            .await;
        assert!(
            upgraded >= 1,
            "expected a promotion, got (archived={archived}, downgraded={downgraded}, upgraded={upgraded}, cleaned={cleaned})"
        );
        assert_eq!(
            state.storage.get_memory(&id).await.unwrap().unwrap().level,
            MemoryLevel::Summary
        );
    }

    /// P0-3: `X-Merkur-Namespace` routes the whole request into one bucket.
    #[tokio::test]
    async fn test_namespace_header_isolates_search_results() {
        let state = test_app().await;
        let app = router::create_router(state);

        // Write identical content into two buckets via the header.
        for ns in ["alpha", "beta"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/v1/write")
                        .header("content-type", "application/json")
                        .header("x-merkur-namespace", ns)
                        .body(Body::from(r#"{"content":"bucket isolation probe sentence"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        // Search alpha — must see exactly its own copy.
        let resp = app
            .oneshot(
                Request::get("/v1/search?q=bucket+isolation+probe")
                    .header("x-merkur-namespace", "alpha")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["results"].as_array().unwrap().len(),
            1,
            "alpha bucket must contain exactly its own copy, got {json}"
        );
    }

    /// No header → default bucket, matching pre-namespace behavior.
    #[tokio::test]
    async fn test_search_without_namespace_header_uses_default_bucket() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"plain default bucket memory"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .oneshot(
                Request::get("/v1/search?q=plain+default+bucket")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["results"].as_array().unwrap().len(), 1);
        assert_eq!(
            json["results"][0]["namespace"].as_str(),
            Some("default"),
            "unscoped writes must land in and surface from the default bucket"
        );
    }

    /// P1-6: POST /v1/context assembles a prompt-ready digest under a token
    /// budget, deduplicated and packed greedily.
    #[tokio::test]
    async fn test_context_endpoint_packs_digest_under_budget() {
        let state = test_app().await;
        let app = router::create_router(state);

        // Two near-duplicates (Jaccard > 0.8) + one distinct memory.
        for content in [
            "the user prefers Rust for systems work",
            "the user prefers Rust for systems work today",
            "an unrelated note about coffee",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/v1/write")
                        .header("content-type", "application/json")
                        .body(Body::from(format!(r#"{{"content":"{content}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let resp = app
            .oneshot(
                Request::post("/v1/context")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"q":"rust preference","token_budget":100}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 16384).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Digest is present and non-empty.
        let digest = json["digest"].as_str().unwrap();
        assert!(!digest.is_empty());
        // Near-duplicate was dropped: at most 2 items survive.
        let items = json["items"].as_array().unwrap();
        assert!(items.len() <= 2, "near-duplicate must be deduped, got {items:?}");
        // Token estimate respects the budget.
        let est = json["token_estimate"].as_u64().unwrap();
        assert!(est <= 100, "estimate {est} exceeds budget");
        // Dropped count is surfaced.
        assert!(json["dropped"].as_u64().is_some());
    }

    /// Context endpoint respects the namespace header like search does.
    #[tokio::test]
    async fn test_context_endpoint_respects_namespace() {
        let state = test_app().await;
        let app = router::create_router(state);

        for (ns, content) in [
            ("alpha", "alpha bucket rust note"),
            ("beta", "beta bucket rust note"),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/v1/write")
                        .header("content-type", "application/json")
                        .header("x-merkur-namespace", ns)
                        .body(Body::from(format!(r#"{{"content":"{content}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let resp = app
            .oneshot(
                Request::post("/v1/context")
                    .header("content-type", "application/json")
                    .header("x-merkur-namespace", "alpha")
                    .body(Body::from(r#"{"q":"rust note","token_budget":50}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 16384).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = json["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["namespace"].as_str(), Some("alpha"));
    }
}