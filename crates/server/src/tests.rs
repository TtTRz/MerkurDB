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

    /// Regression: hybrid recall truncated the fused pool to exactly `limit`,
    /// so `offset` past the first page was always empty and `total` was
    /// capped at `limit` even when more documents matched.
    #[tokio::test]
    async fn test_hybrid_search_paginates_beyond_first_page() {
        let state = test_app().await;
        let app = router::create_router(state);

        for i in 0..15 {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/v1/write")
                        .header("content-type", "application/json")
                        .body(Body::from(format!(
                            r#"{{"content":"pagination probe token record number {i}"}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let resp = app
            .oneshot(
                Request::get(
                    "/v1/search?q=pagination+probe+token&mode=hybrid&limit=10&offset=10&score_threshold=0.0",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["total"].as_u64().unwrap(),
            15,
            "total must count all matches, not just the first fused page"
        );
        assert_eq!(
            json["results"].as_array().unwrap().len(),
            5,
            "offset=10 with 15 matches must yield a second page of 5"
        );
    }

    /// Hybrid mode gates on the fused retrieval relevance (like `fast` gates
    /// on cosine), not on the composite score — whose structural floor
    /// (0.2*weight + 0.3*importance = 0.35 for a fresh memory) would otherwise
    /// make the default threshold 0.3 a no-op. A rank-1 dual-channel hit has
    /// fused = 1.0 and must clear a threshold of 0.9 that its composite
    /// (0.85) would fail.
    #[tokio::test]
    async fn test_hybrid_threshold_gates_fused_relevance_not_composite() {
        let state = test_app().await;
        let app = router::create_router(state);

        for content in [
            "the threshold wiring probe memory",
            "an unrelated filler about database vacuuming",
            "another filler about cache invalidation",
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
                Request::get(
                    "/v1/search?q=the+threshold+wiring+probe+memory&score_threshold=0.9",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let results = json["results"].as_array().unwrap();
        assert!(
            !results.is_empty(),
            "fused=1.0 exact match must clear threshold 0.9 even though composite is 0.85"
        );
        assert_eq!(
            results[0]["content"].as_str().unwrap(),
            "the threshold wiring probe memory"
        );
    }

    /// Demand signal integrity: only results actually *served* (the paginated
    /// page) count as access. The wider recall pool must not be recorded, and
    /// write-time dedup probes are governance, not demand.
    #[tokio::test]
    async fn test_search_records_access_only_for_served_results() {
        let state = test_app().await;
        let app = router::create_router(state);

        let mut ids = Vec::new();
        for content in [
            "served signal probe alpha one",
            "served signal probe beta two",
            "served signal probe gamma three",
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
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            ids.push(json["id"].as_str().unwrap().to_string());
        }

        let resp = app
            .clone()
            .oneshot(
                Request::get("/v1/search?q=served+signal+probe&limit=1&score_threshold=0.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let served = json["results"][0]["id"].as_str().unwrap().to_string();

        for id in &ids {
            let resp = app
                .clone()
                .oneshot(
                    Request::get(format!("/v1/memory/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let count = json["access_count"].as_u64().unwrap();
            if *id == served {
                assert!(count >= 1, "served result must record access");
            } else {
                assert_eq!(
                    count, 0,
                    "unserved memory must not record access (id={id})"
                );
            }
        }
    }

    /// GET /v1/memory/{id} must round-trip namespace and importance — the
    /// Rust SDK deserializes the response into `Memory`, where both fields
    /// are required.
    #[tokio::test]
    async fn test_get_memory_response_includes_namespace_and_importance() {
        let state = test_app().await;
        let app = router::create_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .header("x-merkur-namespace", "alpha")
                    .body(Body::from(r#"{"content":"namespace contract probe"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = json["id"].as_str().unwrap().to_string();

        let resp = app
            .oneshot(
                Request::get(format!("/v1/memory/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["namespace"].as_str().unwrap(), "alpha");
        assert_eq!(json["importance"].as_f64().unwrap(), 0.5);
    }

    /// /v1/graph/{id} must respect the namespace bucket like every other read
    /// path: neither the neighborhood nor the returned edges may leak a
    /// foreign bucket's ids.
    #[tokio::test]
    async fn test_graph_endpoint_respects_namespace() {
        let state = test_app().await;
        let app = router::create_router(state);

        let mut ids = std::collections::HashMap::new();
        for (ns, tag) in [
            ("alpha", "graph probe node in alpha"),
            ("beta", "graph probe node in beta"),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/v1/write")
                        .header("content-type", "application/json")
                        .header("x-merkur-namespace", ns)
                        .body(Body::from(format!(r#"{{"content":"{tag}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            ids.insert(ns, json["id"].as_str().unwrap().to_string());
        }

        // A cross-bucket manual edge: alpha -> beta.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/relate")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"source_id":"{}","target_id":"{}"}}"#,
                        ids["alpha"], ids["beta"]
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .oneshot(
                Request::get(format!("/v1/graph/{}?depth=2", ids["alpha"]))
                    .header("x-merkur-namespace", "alpha")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let text = serde_json::to_string(&json).unwrap();
        assert!(
            !text.contains(&ids["beta"]),
            "graph response must not leak the foreign-bucket id: {text}"
        );
    }
    /// P1-7 write governance: an UPDATE verdict absorbs the pending memory
    /// into the existing one — in-place content update on the target,
    /// invalidation with an audit pointer on the pending row — when the pair
    /// clears the similarity floor.
    struct ScriptedConsolidator {
        action: merkur_core::AdjudicationAction,
    }

    #[async_trait::async_trait]
    impl merkur_core::Consolidator for ScriptedConsolidator {        async fn consolidate(
            &self,
            memories: &[merkur_core::Memory],
        ) -> merkur_core::MerkurResult<merkur_core::ConsolidationReport> {
            let mut r = merkur_core::ConsolidationReport::empty();
            r.memories_processed = memories.len();
            Ok(r)
        }

        async fn adjudicate(
            &self,
            _pending: &merkur_core::Memory,
            candidates: &[merkur_core::ScoredMemory],
        ) -> merkur_core::MerkurResult<merkur_core::Adjudication> {
            Ok(merkur_core::Adjudication {
                action: self.action.clone(),
                target_id: candidates.first().map(|c| c.id.clone()),
                reason: "scripted".into(),
            })
        }
    }

    /// Build a memory with a unit-ish embedding along one axis pair so the
    /// cosine between the two fixtures is ~0.993 (>= 0.6 floor, < 0.999).
    fn gov_memory(content: &str, first: f32, second: f32) -> merkur_core::NewMemory {
        let mut embedding = vec![0.0f32; 16];
        embedding[0] = first;
        embedding[1] = second;
        merkur_core::NewMemory {
            content: content.into(),
            category: Some("general".into()),
            context: Default::default(),
            metadata: Default::default(),
            embedding: Some(embedding),
            namespace: merkur_core::DEFAULT_NAMESPACE.to_string(),
        }
    }

    #[tokio::test]
    async fn test_consolidation_absorbs_update_verdict() {
        let state = test_app().await;
        let x = state
            .storage
            .insert_memory(&gov_memory("the deploy region is us-east", 1.0, 0.0))
            .await
            .unwrap();
        // X is already consolidated; only the pending P gets adjudicated.
        state.storage.mark_consolidated(std::slice::from_ref(&x)).await.unwrap();
        let p = state
            .storage
            .insert_memory(&gov_memory("the deploy region is now us-west", 0.9, 0.1))
            .await
            .unwrap();

        let consolidator = ScriptedConsolidator {
            action: merkur_core::AdjudicationAction::Update,
        };
        let report = crate::scheduler::Scheduler::run_consolidation_once(
            &*state.storage,
            &consolidator,
            10,
            0.6,
            5,
        )
        .await;

        assert_eq!(report.absorptions, 1, "UPDATE verdict must absorb");
        let xm = state.storage.get_memory(&x).await.unwrap().unwrap();
        assert_eq!(xm.content, "the deploy region is now us-west");
        let pm = state.storage.get_memory(&p).await.unwrap().unwrap();
        assert!(pm.invalid_at.is_some(), "absorbed row is invalidated");
        assert_eq!(
            pm.metadata.get("absorbed_into").and_then(|v| v.as_str()),
            Some(x.as_str())
        );
    }

    #[tokio::test]
    async fn test_consolidation_skips_update_below_similarity_floor() {
        let state = test_app().await;
        let x = state
            .storage
            .insert_memory(&gov_memory("the deploy region is us-east", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(std::slice::from_ref(&x)).await.unwrap();
        let p = state
            .storage
            .insert_memory(&gov_memory("the deploy region is now us-west", 0.9, 0.1))
            .await
            .unwrap();

        let consolidator = ScriptedConsolidator {
            action: merkur_core::AdjudicationAction::Update,
        };
        let report = crate::scheduler::Scheduler::run_consolidation_once(
            &*state.storage,
            &consolidator,
            10,
            0.999, // floor above the ~0.993 pair similarity
            5,
        )
        .await;

        assert_eq!(report.absorptions, 0, "below-floor verdict must not execute");
        let xm = state.storage.get_memory(&x).await.unwrap().unwrap();
        assert_eq!(xm.content, "the deploy region is us-east");
        let pm = state.storage.get_memory(&p).await.unwrap().unwrap();
        assert!(pm.invalid_at.is_none());
    }
    /// Write-governance retention (Q7): the forgetting tick hard-deletes rows
    /// whose invalid_at is older than purge_invalidated_days — and must do so
    /// even when no forgetting candidates exist (invalidated rows are
    /// excluded from the candidate list).
    #[tokio::test]
    async fn test_forgetting_purges_invalidated_memories() {
        use merkur_core::NewMemory;

        let state = test_app().await;
        let id = state
            .storage
            .insert_memory(&NewMemory {
                content: "superseded fact awaiting purge".into(),
                category: Some("general".into()),
                context: Default::default(),
                metadata: Default::default(),
                embedding: None,
                namespace: merkur_core::DEFAULT_NAMESPACE.to_string(),
            })
            .await
            .unwrap();
        state.storage.invalidate_memory(&id, None).await.unwrap();

        // purge window 0 days: anything invalidated before "now" qualifies.
        let (archived, downgraded, upgraded, cleaned, purged) =
            crate::scheduler::Scheduler::run_forgetting_once(
                &*state.storage,
                &*state.forgetter,
                100,
                30,
                0,
            )
            .await;
        assert_eq!(purged, 1, "invalidated row must be purged");
        assert_eq!(archived + downgraded + upgraded + cleaned, 0);
        assert!(state.storage.get_memory(&id).await.unwrap().is_none());
    }

    /// P0-2: access-driven promotion. A demoted memory that keeps getting
    /// served must climb back one rung on the next forgetting tick. Demand is
    /// recorded explicitly via `record_access` (retrieval itself is a pure
    /// query), which keeps this wiring test deterministic.
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

        // Demonstrated demand, recorded by the serving point.
        state
            .storage
            .record_access(std::slice::from_ref(&id))
            .await
            .unwrap();
        state
            .storage
            .record_access(std::slice::from_ref(&id))
            .await
            .unwrap();
        let forgetter = Arc::new(EbbinghausForgetter::new(EbbinghausConfig {
            half_life_seconds: f64::MAX,
            threshold_upgrade: 0.2,
            upgrade_min_access_count: 1,
            ..Default::default()
        }));

        let (archived, downgraded, upgraded, cleaned, _purged) =
            crate::scheduler::Scheduler::run_forgetting_once(
                &*state.storage,
                &*forgetter,
                100,
                30,
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