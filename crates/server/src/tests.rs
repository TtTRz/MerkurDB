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
        test_app_with_db(&temp_db_path()).await
    }

    async fn test_app_with_db(path: &str) -> AppState {
        let dim = 16;
        let embedder: Arc<dyn merkur_core::Embedder> = Arc::new(NoopEmbedder::new(dim));
        let storage: Arc<dyn merkur_core::Storage> =
            Arc::new(SqliteStorage::new(path, dim).expect("Failed to create test storage"));
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
    async fn test_deep_search_includes_vector_seeds() {
        let state = test_app().await;
        let app = router::create_router(state);

        // No edges exist: diffusion has nothing to expand, so anything
        // returned must be a vector seed. Regression: deep mode used to
        // drop its own seeds (BFS anchor filtered at d > 0), answering
        // "neighbors of the answer" instead of the answer.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"deep seed inclusion probe"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = app
            .oneshot(
                Request::get(
                    "/v1/search?q=deep+seed+inclusion+probe&mode=deep&score_threshold=0.0",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = json["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert!(
            ids.contains(&id.as_str()),
            "deep mode must serve its own vector seeds; got {ids:?}"
        );
    }

    #[tokio::test]
    async fn test_graph_accepts_namespace_query_param() {
        let state = test_app().await;
        let app = router::create_router(state);

        let write_beta = |content: &str| {
            let app = app.clone();
            let content = content.to_string();
            async move {
                let resp = app
                    .oneshot(
                        Request::post("/v1/write")
                            .header("content-type", "application/json")
                            .header("x-merkur-namespace", "beta")
                            .body(Body::from(format!(r#"{{"content":"{content}"}}"#)))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::CREATED);
                let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            }
        };
        let a = write_beta("beta-adjacent fact A").await;
        let b = write_beta("beta-adjacent fact B").await;

        let resp = app
            .clone()
            .oneshot(
                Request::post("/v1/relate")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "source_id": a, "target_id": b, "relation": "mentions", "weight": 0.9
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Default bucket (no header, no param): the beta neighborhood is
        // out of scope.
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/graph/{a}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["neighborhood"].as_array().unwrap().len(), 0);

        // The query param overrides the bucket, same as /v1/memories.
        let resp = app
            .oneshot(
                Request::get(format!("/v1/graph/{a}?namespace=beta"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["neighborhood"].as_array().unwrap().len(), 1);
        assert_eq!(json["edges"].as_array().unwrap().len(), 1);
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
        assert_eq!(
            results[0]["content"].as_str().unwrap(),
            "trigram bm25 exact match target sentence"
        );
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
                Request::get("/v1/search?q=the+threshold+wiring+probe+memory&score_threshold=0.9")
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
                assert_eq!(count, 0, "unserved memory must not record access (id={id})");
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
    struct PausedAdjudicator {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Consolidator for PausedAdjudicator {
        async fn consolidate(
            &self,
            memories: &[merkur_core::Memory],
        ) -> merkur_core::MerkurResult<merkur_core::ConsolidationReport> {
            let mut report = merkur_core::ConsolidationReport::empty();
            report.memories_processed = memories.len();
            report.new_abstracts = memories
                .iter()
                .map(|m| (m.id.clone(), "summary".into()))
                .collect();
            Ok(report)
        }

        async fn adjudicate(
            &self,
            _: &merkur_core::Memory,
            _: &[merkur_core::ScoredMemory],
        ) -> merkur_core::MerkurResult<merkur_core::Adjudication> {
            self.entered.notify_one();
            self.release.notified().await;
            if self.fail {
                Err(merkur_core::MerkurError::Consolidation(
                    "test failure".into(),
                ))
            } else {
                Ok(merkur_core::Adjudication::default())
            }
        }
    }

    #[tokio::test]
    async fn mixed_batch_without_embedding_finishes_consolidation() {
        let mut state = test_app().await;
        let old = state
            .storage
            .insert_memory(&gov_memory("existing candidate", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(&[old]).await.unwrap();

        let mut text_only = gov_memory("text-only memory", 1.0, 0.0);
        text_only.embedding = None;
        let text_id = state.storage.insert_memory(&text_only).await.unwrap();
        let vector_id = state
            .storage
            .insert_memory(&gov_memory("vector memory", 0.9, 0.1))
            .await
            .unwrap();

        let consolidator = Arc::new(PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: false,
        });
        state.consolidator = consolidator.clone();
        let mut config = (*state.config).clone();
        config.consolidation.adjudication_candidates = 5;
        state.config = Arc::new(config);
        // Only the vector-backed memory should reach adjudication.
        consolidator.release.notify_one();
        let report = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.start_consolidation().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            consolidator.entered.notified(),
        )
        .await
        .expect("vector-backed memory must still be adjudicated");
        assert_eq!(report.memories_processed, 2);
        assert_eq!(
            report.errors, 0,
            "absent optional embedding is not an adjudication error"
        );
        for id in [&text_id, &vector_id] {
            let memory = state.storage.get_memory(id).await.unwrap().unwrap();
            assert_eq!(memory.abstract_.as_deref(), Some("summary"));
            assert!(!memory.pending_consolidation);
        }
        assert_eq!(
            state.storage.stats().await.unwrap().pending_consolidation,
            0
        );
        assert_eq!(state.consolidation.snapshot().state, "succeeded");
        assert_eq!(
            state.storage.get_consolidation_log(1).await.unwrap()[0].errors,
            0
        );
    }

    #[tokio::test]
    async fn pending_remains_visible_until_adjudication_finishes() {
        let state = test_app().await;
        let old = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(&[old]).await.unwrap();
        let id = state
            .storage
            .insert_memory(&gov_memory("new", 0.9, 0.1))
            .await
            .unwrap();
        let consolidator = Arc::new(PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: false,
        });
        let task = tokio::spawn({
            let storage = state.storage.clone();
            let c = consolidator.clone();
            async move {
                crate::scheduler::Scheduler::run_consolidation_once(&*storage, &*c, 10, 0.6, 5)
                    .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consolidator.entered.notified(),
        )
        .await
        .unwrap();
        assert!(
            state
                .storage
                .get_memory(&id)
                .await
                .unwrap()
                .unwrap()
                .pending_consolidation
        );
        consolidator.release.notify_one();
        assert_eq!(task.await.unwrap().errors, 0);
        assert!(
            !state
                .storage
                .get_memory(&id)
                .await
                .unwrap()
                .unwrap()
                .pending_consolidation
        );
    }

    #[tokio::test]
    async fn archived_pending_cannot_starve_later_consolidation_batches() {
        let state = test_app().await;
        let archived = state
            .storage
            .insert_memory(&gov_memory("archived first", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.update_level(&archived, -1).await.unwrap();
        let live = state
            .storage
            .insert_memory(&gov_memory("live second", 0.9, 0.1))
            .await
            .unwrap();
        let c = PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: false,
        };
        let mut processed = Vec::new();
        // Disable vector adjudication: extraction and completion use the real scheduler path.
        for _ in 0..3 {
            let report =
                crate::scheduler::Scheduler::run_consolidation_once(&*state.storage, &c, 1, 0.6, 0)
                    .await;
            assert_eq!(report.errors, 0);
            processed.push(report.memories_processed);
        }
        assert_eq!(
            processed,
            vec![1, 0, 0],
            "archived rows must not be claimed on every tick"
        );
        assert!(
            !state
                .storage
                .get_memory(&live)
                .await
                .unwrap()
                .unwrap()
                .pending_consolidation
        );
        assert!(
            state
                .storage
                .get_memory(&archived)
                .await
                .unwrap()
                .unwrap()
                .pending_consolidation
        );
        assert_eq!(
            state.storage.stats().await.unwrap().pending_consolidation,
            0
        );
    }

    #[tokio::test]
    async fn edits_during_adjudication_remain_pending() {
        let state = test_app().await;
        let old = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(&[old]).await.unwrap();
        let id = state
            .storage
            .insert_memory(&gov_memory("new", 0.9, 0.1))
            .await
            .unwrap();
        let c = Arc::new(PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: false,
        });
        let job = tokio::spawn({
            let storage = state.storage.clone();
            let c = c.clone();
            async move {
                crate::scheduler::Scheduler::run_consolidation_once(&*storage, &*c, 10, 0.6, 5)
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), c.entered.notified())
            .await
            .unwrap();
        state
            .storage
            .update_memory(&id, "edited while processing", Some(&[1.0; 16]))
            .await
            .unwrap();
        c.release.notify_one();
        assert_eq!(job.await.unwrap().errors, 0);
        let memory = state.storage.get_memory(&id).await.unwrap().unwrap();
        assert_eq!(memory.content, "edited while processing");
        assert!(
            memory.pending_consolidation,
            "a stale batch must not acknowledge new content"
        );
    }

    struct PausedVerdict {
        pause: PausedAdjudicator,
        action: merkur_core::AdjudicationAction,
        delete_pending: bool,
    }

    #[async_trait::async_trait]
    impl Consolidator for PausedVerdict {
        async fn consolidate(
            &self,
            memories: &[merkur_core::Memory],
        ) -> merkur_core::MerkurResult<merkur_core::ConsolidationReport> {
            self.pause.consolidate(memories).await
        }

        async fn adjudicate(
            &self,
            pending: &merkur_core::Memory,
            candidates: &[merkur_core::ScoredMemory],
        ) -> merkur_core::MerkurResult<merkur_core::Adjudication> {
            self.pause.adjudicate(pending, candidates).await?;
            Ok(merkur_core::Adjudication {
                action: self.action.clone(),
                target_id: if self.delete_pending {
                    Some(pending.id.clone())
                } else {
                    candidates.first().map(|c| c.id.clone())
                },
                reason: "paused verdict".into(),
            })
        }
    }

    async fn stale_verdict_case(
        action: merkur_core::AdjudicationAction,
        change: &str,
        delete_pending: bool,
    ) {
        let state = test_app().await;
        let target = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state
            .storage
            .mark_consolidated(std::slice::from_ref(&target))
            .await
            .unwrap();
        let pending = state
            .storage
            .insert_memory(&gov_memory("new", 0.9, 0.1))
            .await
            .unwrap();
        let c = Arc::new(PausedVerdict {
            pause: PausedAdjudicator {
                entered: Default::default(),
                release: Default::default(),
                fail: false,
            },
            action,
            delete_pending,
        });
        let job = tokio::spawn({
            let storage = state.storage.clone();
            let c = c.clone();
            async move {
                crate::scheduler::Scheduler::run_consolidation_once(&*storage, &*c, 10, 0.6, 5)
                    .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            c.pause.entered.notified(),
        )
        .await
        .unwrap();
        match change {
            "edit_pending" | "pending_aba" => {
                state
                    .storage
                    .update_memory(&pending, "edited", Some(&[1.0; 16]))
                    .await
                    .unwrap();
                if change == "pending_aba" {
                    state
                        .storage
                        .update_memory(&pending, "new", Some(&[1.0; 16]))
                        .await
                        .unwrap();
                }
            }
            "edit_target" | "target_aba" => {
                state
                    .storage
                    .update_memory(&target, "edited", Some(&[1.0; 16]))
                    .await
                    .unwrap();
                if change == "target_aba" {
                    state
                        .storage
                        .update_memory(&target, "old", Some(&[1.0; 16]))
                        .await
                        .unwrap();
                }
            }
            "delete_target" => state.storage.delete_memory(&target).await.unwrap(),
            "delete_pending" => state.storage.delete_memory(&pending).await.unwrap(),
            "invalidate_target" => state
                .storage
                .invalidate_memory(&target, None)
                .await
                .unwrap(),
            "invalidate_pending" => state
                .storage
                .invalidate_memory(&pending, None)
                .await
                .unwrap(),
            _ => unreachable!(),
        }
        c.pause.release.notify_one();
        let report = job.await.unwrap();
        assert_eq!(
            report.absorptions, 0,
            "stale absorption executed after {change}"
        );
        assert_eq!(
            report.invalidations, 0,
            "stale DELETE executed after {change}"
        );
        let source = state.storage.get_memory(&pending).await.unwrap();
        if change == "delete_pending" {
            assert!(source.is_none());
        } else if change != "invalidate_pending" {
            let source = source.unwrap();
            assert!(
                source.invalid_at.is_none(),
                "stale verdict invalidated source after {change}"
            );
            assert!(
                source.pending_consolidation,
                "conflicting work must be retried after {change}"
            );
        }
        if let Some(target) = state.storage.get_memory(&target).await.unwrap() {
            assert_eq!(
                target.content,
                if change == "edit_target" {
                    "edited"
                } else {
                    "old"
                }
            );
            assert_eq!(target.invalid_at.is_some(), change == "invalidate_target");
        }
    }

    #[tokio::test]
    async fn adjudication_rejects_stale_update() {
        for change in [
            "edit_target",
            "edit_pending",
            "pending_aba",
            "target_aba",
            "delete_target",
            "delete_pending",
            "invalidate_target",
            "invalidate_pending",
        ] {
            stale_verdict_case(merkur_core::AdjudicationAction::Update, change, false).await;
        }
    }

    #[tokio::test]
    async fn adjudication_rejects_stale_delete() {
        for delete_pending in [false, true] {
            for change in [
                "edit_target",
                "edit_pending",
                "pending_aba",
                "target_aba",
                "delete_target",
                "delete_pending",
                "invalidate_target",
                "invalidate_pending",
            ] {
                stale_verdict_case(
                    merkur_core::AdjudicationAction::Delete,
                    change,
                    delete_pending,
                )
                .await;
            }
        }
    }

    async fn delete_with_nonfirst_candidate_change(
        storage: Arc<dyn merkur_core::Storage>,
        change: &str,
        delete_pending: bool,
    ) {
        let first = storage
            .insert_memory(&gov_memory("related fact", 1.0, 0.0))
            .await
            .unwrap();
        let second = storage
            .insert_memory(&gov_memory("contradicting evidence", 0.9, 0.3))
            .await
            .unwrap();
        storage
            .mark_consolidated(&[first.clone(), second.clone()])
            .await
            .unwrap();
        let memory = gov_memory("new fact", 1.0, 0.0);
        let pending = storage.insert_memory(&memory).await.unwrap();
        let candidates: Vec<_> = storage
            .vector_search_ns(memory.embedding.as_deref().unwrap(), "default", 3)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.id != pending)
            .map(|c| c.id)
            .collect();
        assert_eq!(candidates, [first.clone(), second.clone()]);
        let c = Arc::new(PausedVerdict {
            pause: PausedAdjudicator {
                entered: Default::default(),
                release: Default::default(),
                fail: false,
            },
            action: merkur_core::AdjudicationAction::Delete,
            delete_pending,
        });
        let job = tokio::spawn({
            let storage = storage.clone();
            let c = c.clone();
            async move {
                crate::scheduler::Scheduler::run_consolidation_once(&*storage, &*c, 10, 0.6, 5)
                    .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            c.pause.entered.notified(),
        )
        .await
        .unwrap();
        match change {
            "edit" | "edit_restore" => {
                storage
                    .update_memory(&second, "corrected evidence", Some(&[0.0; 16]))
                    .await
                    .unwrap();
                if change == "edit_restore" {
                    let original = gov_memory("contradicting evidence", 0.9, 0.3);
                    storage
                        .update_memory(&second, &original.content, original.embedding.as_deref())
                        .await
                        .unwrap();
                }
            }
            "delete" => storage.delete_memory(&second).await.unwrap(),
            "invalidate" => storage.invalidate_memory(&second, None).await.unwrap(),
            "archive" => storage.update_level(&second, -1).await.unwrap(),
            "unchanged" => {}
            _ => unreachable!(),
        }
        c.pause.release.notify_one();
        let report = job.await.unwrap();
        let conflict = delete_pending && change != "unchanged";
        assert_eq!(
            report.invalidations,
            usize::from(!conflict),
            "unexpected DELETE outcome after {change}, self={delete_pending}"
        );
        assert_eq!(report.errors, 0, "a revision conflict is not a batch error");
        let current = storage.get_memory(&pending).await.unwrap().unwrap();
        assert_eq!(current.invalid_at.is_some(), delete_pending && !conflict);
        if conflict {
            assert!(
                current.pending_consolidation,
                "conflicting work must remain retryable"
            );
        }
        if !delete_pending {
            assert!(
                !current.pending_consolidation,
                "the unchanged source must finish"
            );
        }
        let first = storage.get_memory(&first).await.unwrap().unwrap();
        assert_eq!(first.invalid_at.is_some(), !delete_pending && !conflict);
        assert_eq!(first.content, "related fact");
    }

    #[tokio::test]
    async fn self_delete_retries_when_nonfirst_candidate_changes() {
        for change in [
            "edit",
            "edit_restore",
            "delete",
            "invalidate",
            "archive",
            "unchanged",
        ] {
            delete_with_nonfirst_candidate_change(test_app().await.storage, change, true).await;
        }
    }

    #[tokio::test]
    async fn candidate_delete_ignores_unrelated_evidence_changes() {
        delete_with_nonfirst_candidate_change(test_app().await.storage, "edit", false).await;
    }

    #[cfg(feature = "lancedb")]
    #[tokio::test]
    async fn lance_delete_guards_original_candidates() {
        for change in [
            "edit",
            "edit_restore",
            "delete",
            "invalidate",
            "archive",
            "unchanged",
        ] {
            for delete_pending in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let storage = merkur_storage::LanceDbStorage::new(
                    dir.path().join("lance").to_str().unwrap(),
                    dir.path().join("meta.db").to_str().unwrap(),
                    16,
                )
                .await
                .unwrap();
                delete_with_nonfirst_candidate_change(Arc::new(storage), change, delete_pending)
                    .await;
            }
        }
    }

    #[tokio::test]
    async fn gathered_verdicts_cannot_overwrite_an_earlier_absorption() {
        struct SameTarget(String);
        #[async_trait::async_trait]
        impl Consolidator for SameTarget {
            async fn consolidate(
                &self,
                memories: &[merkur_core::Memory],
            ) -> merkur_core::MerkurResult<merkur_core::ConsolidationReport> {
                let mut report = merkur_core::ConsolidationReport::empty();
                report.memories_processed = memories.len();
                report.new_abstracts = memories
                    .iter()
                    .map(|m| (m.id.clone(), "summary".into()))
                    .collect();
                Ok(report)
            }
            async fn adjudicate(
                &self,
                _: &merkur_core::Memory,
                candidates: &[merkur_core::ScoredMemory],
            ) -> merkur_core::MerkurResult<merkur_core::Adjudication> {
                assert!(candidates.iter().any(|c| c.id == self.0));
                Ok(merkur_core::Adjudication {
                    action: merkur_core::AdjudicationAction::Update,
                    target_id: Some(self.0.clone()),
                    reason: "same target".into(),
                })
            }
        }
        let state = test_app().await;
        let target = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state
            .storage
            .mark_consolidated(std::slice::from_ref(&target))
            .await
            .unwrap();
        let a = state
            .storage
            .insert_memory(&gov_memory("first", 0.9, 0.1))
            .await
            .unwrap();
        let b = state
            .storage
            .insert_memory(&gov_memory("second", 0.8, 0.2))
            .await
            .unwrap();
        let report = crate::scheduler::Scheduler::run_consolidation_once(
            &*state.storage,
            &SameTarget(target.clone()),
            10,
            0.6,
            5,
        )
        .await;
        assert_eq!(report.errors, 0);
        assert_eq!(
            report.absorptions, 1,
            "the second original target revision must conflict"
        );
        let a = state.storage.get_memory(&a).await.unwrap().unwrap();
        let b = state.storage.get_memory(&b).await.unwrap().unwrap();
        assert_ne!(a.invalid_at.is_some(), b.invalid_at.is_some());
        let survivor = if a.invalid_at.is_none() { a } else { b };
        assert!(
            survivor.pending_consolidation,
            "conflicted source must remain pending"
        );
        assert!(
            state
                .storage
                .get_memory(&target)
                .await
                .unwrap()
                .unwrap()
                .pending_consolidation
        );
    }

    #[tokio::test]
    async fn consolidation_rejects_overlap_and_survives_waiter_cancellation() {
        let mut state = test_app().await;
        let old = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(&[old]).await.unwrap();
        state
            .storage
            .insert_memory(&gov_memory("new", 0.9, 0.1))
            .await
            .unwrap();
        let c = Arc::new(PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: false,
        });
        state.consolidator = c.clone();
        let mut config = (*state.config).clone();
        config.consolidation.adjudication_candidates = 5;
        state.config = Arc::new(config);
        // This is the same entry point used by the periodic scheduler.
        let task = state.start_consolidation().expect("first run starts");
        tokio::time::timeout(std::time::Duration::from_secs(5), c.entered.notified())
            .await
            .unwrap();
        let app = router::create_router(state.clone());
        let response = app
            .clone()
            .oneshot(Request::get("/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["consolidation"]["state"], "running");
        assert_eq!(body["pending_consolidation"], 1);
        let response = app
            .oneshot(
                Request::post("/v1/consolidate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        drop(task); // A disconnected HTTP waiter must not cancel the owned job.
        c.release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.consolidation.wait_idle(),
        )
        .await
        .unwrap();
        assert_eq!(state.consolidation.snapshot().state, "succeeded");
        assert_eq!(
            state.storage.stats().await.unwrap().pending_consolidation,
            0
        );
        assert_eq!(
            state.storage.get_consolidation_log(10).await.unwrap().len(),
            1
        );
        assert!(state.start_consolidation().unwrap().await.is_ok());
    }

    #[tokio::test]
    async fn failed_adjudication_keeps_memory_pending_and_reports_failure() {
        let mut state = test_app().await;
        let old = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(&[old]).await.unwrap();
        let id = state
            .storage
            .insert_memory(&gov_memory("new", 0.9, 0.1))
            .await
            .unwrap();
        let c = Arc::new(PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: true,
        });
        state.consolidator = c.clone();
        let mut config = (*state.config).clone();
        config.consolidation.adjudication_candidates = 5;
        state.config = Arc::new(config);
        c.release.notify_one();
        let report = state.start_consolidation().unwrap().await.unwrap();
        assert_eq!(report.errors, 1);
        assert_eq!(state.consolidation.snapshot().state, "failed");
        assert!(
            state
                .storage
                .get_memory(&id)
                .await
                .unwrap()
                .unwrap()
                .pending_consolidation
        );
        assert_eq!(
            state.storage.get_consolidation_log(1).await.unwrap()[0].errors,
            1
        );
        // Failure must release the execution gate for a subsequent retry.
        c.release.notify_one();
        assert_eq!(
            state.start_consolidation().unwrap().await.unwrap().errors,
            1
        );
    }

    struct BrokenConsolidator {
        panic: bool,
    }
    #[async_trait::async_trait]
    impl Consolidator for BrokenConsolidator {
        async fn consolidate(
            &self,
            _: &[merkur_core::Memory],
        ) -> merkur_core::MerkurResult<merkur_core::ConsolidationReport> {
            assert!(!self.panic, "scripted panic");
            Err(merkur_core::MerkurError::Consolidation(
                "scripted failure".into(),
            ))
        }
    }

    #[tokio::test]
    async fn extraction_error_or_panic_reports_failure_and_releases_gate() {
        for panic in [false, true] {
            let mut state = test_app().await;
            state
                .storage
                .insert_memory(&gov_memory("pending", 1.0, 0.0))
                .await
                .unwrap();
            state.consolidator = Arc::new(BrokenConsolidator { panic });
            let result = state.start_consolidation().unwrap().await;
            if panic {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().errors, 1);
            }
            let status = state.consolidation.snapshot();
            assert_eq!(status.state, "failed");
            assert!(status.finished_at.is_some());
            assert!(status.errors > 0);
            assert_eq!(
                state.storage.stats().await.unwrap().pending_consolidation,
                1
            );
            let response = router::create_router(state.clone())
                .oneshot(
                    Request::post("/v1/consolidate")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if panic {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            );
            let body: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 8192)
                    .await
                    .unwrap(),
            )
            .unwrap();
            if !panic {
                assert_eq!(body["status"], "failed");
            }
        }
    }

    struct ObservedForgetter {
        calls: AtomicU64,
    }
    impl Forgetter for ObservedForgetter {
        fn compute_weight(&self, _: &merkur_core::Memory, _: chrono::DateTime<chrono::Utc>) -> f64 {
            1.0
        }
        fn decide(
            &self,
            _: &merkur_core::Memory,
            _: chrono::DateTime<chrono::Utc>,
        ) -> merkur_core::LevelAction {
            self.calls.fetch_add(1, Ordering::SeqCst);
            merkur_core::LevelAction::Keep
        }
    }

    #[tokio::test]
    async fn scheduled_consolidation_does_not_block_forgetting_and_shutdown_drains_it() {
        // Shared-memory SQLite cannot use WAL and has different lock semantics.
        // Exercise overlapping jobs with the server's file-backed storage mode.
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("scheduler.db");
        let mut state = test_app_with_db(path.to_str().unwrap()).await;
        let old = state
            .storage
            .insert_memory(&gov_memory("old", 1.0, 0.0))
            .await
            .unwrap();
        state.storage.mark_consolidated(&[old]).await.unwrap();
        state
            .storage
            .insert_memory(&gov_memory("new", 0.9, 0.1))
            .await
            .unwrap();
        let c = Arc::new(PausedAdjudicator {
            entered: Default::default(),
            release: Default::default(),
            fail: false,
        });
        let f = Arc::new(ObservedForgetter {
            calls: AtomicU64::new(0),
        });
        state.consolidator = c.clone();
        state.forgetter = f.clone();
        let mut config = (*state.config).clone();
        config.consolidation.adjudication_candidates = 5;
        config.consolidation.interval_seconds = 60;
        config.forgetting.interval_seconds = 1;
        state.config = Arc::new(config);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let scheduler = Arc::new(crate::scheduler::Scheduler::new(state.clone()));
        let run = tokio::spawn(scheduler.run(rx));
        tokio::time::timeout(std::time::Duration::from_secs(10), c.entered.notified())
            .await
            .unwrap();
        assert!(state.start_consolidation().is_none());
        let before = f.calls.load(Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while f.calls.load(Ordering::SeqCst) == before {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("forgetting stalled behind consolidation");
        tx.send(true).unwrap();
        tokio::task::yield_now().await;
        assert!(!run.is_finished());
        c.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.consolidation.snapshot().state, "succeeded");
    }

    /// P1-7 write governance: an UPDATE verdict absorbs the pending memory
    /// into the existing one — in-place content update on the target,
    /// invalidation with an audit pointer on the pending row — when the pair
    /// clears the similarity floor.
    struct ScriptedConsolidator {
        action: merkur_core::AdjudicationAction,
    }

    #[async_trait::async_trait]
    impl merkur_core::Consolidator for ScriptedConsolidator {
        async fn consolidate(
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
        state
            .storage
            .mark_consolidated(std::slice::from_ref(&x))
            .await
            .unwrap();
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
        state
            .storage
            .mark_consolidated(std::slice::from_ref(&x))
            .await
            .unwrap();
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

        assert_eq!(
            report.absorptions, 0,
            "below-floor verdict must not execute"
        );
        let xm = state.storage.get_memory(&x).await.unwrap().unwrap();
        assert_eq!(xm.content, "the deploy region is us-east");
        let pm = state.storage.get_memory(&p).await.unwrap().unwrap();
        assert!(pm.invalid_at.is_none());
    }

    /// Below-floor candidate sets can never execute a verdict (the dual-signal
    /// rule gates execution on cosine >= floor), so the LLM adjudication call
    /// must be skipped entirely — it is pure cost.
    #[tokio::test]
    async fn test_consolidation_skips_adjudication_call_below_floor() {
        struct CountingConsolidator {
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait::async_trait]
        impl merkur_core::Consolidator for CountingConsolidator {
            async fn consolidate(
                &self,
                _memories: &[merkur_core::Memory],
            ) -> merkur_core::MerkurResult<merkur_core::ConsolidationReport> {
                Ok(merkur_core::ConsolidationReport::empty())
            }

            async fn adjudicate(
                &self,
                _pending: &merkur_core::Memory,
                _candidates: &[merkur_core::ScoredMemory],
            ) -> merkur_core::MerkurResult<merkur_core::Adjudication> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(merkur_core::Adjudication::default())
            }
        }

        let state = test_app().await;
        // Orthogonal axes -> cosine ~0.0, far below the 0.6 floor.
        let x = state
            .storage
            .insert_memory(&gov_memory("the deploy region is us-east", 1.0, 0.0))
            .await
            .unwrap();
        state
            .storage
            .mark_consolidated(std::slice::from_ref(&x))
            .await
            .unwrap();
        let _p = state
            .storage
            .insert_memory(&gov_memory("favorite tea is chamomile", 0.0, 1.0))
            .await
            .unwrap();

        let consolidator = CountingConsolidator {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let _report = crate::scheduler::Scheduler::run_consolidation_once(
            &*state.storage,
            &consolidator,
            10,
            0.6,
            5,
        )
        .await;

        assert_eq!(
            consolidator.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no candidate clears the floor; adjudication must not be called"
        );
    }
    #[cfg(feature = "lancedb")]
    #[tokio::test]
    async fn forgetting_collects_orphan_vectors_when_no_memories_remain() {
        use merkur_core::Storage;
        let dir = tempfile::tempdir_in(".").unwrap();
        let sqlite = dir.path().join("meta.db").to_string_lossy().to_string();
        let lance = dir.path().join("lance").to_string_lossy().to_string();
        let storage = Arc::new(
            merkur_storage::LanceDbStorage::new(&lance, &sqlite, 16)
                .await
                .unwrap(),
        );
        let id = storage
            .insert_memory(&gov_memory("orphaned vector", 1.0, 0.0))
            .await
            .unwrap();
        // Simulate a pre-ledger failed dual write or metadata recovery.
        let pool = merkur_storage::sqlite_helpers::build_pool(&sqlite, 1).unwrap();
        let conn = pool.get().unwrap();
        conn.execute("DELETE FROM memories WHERE id = ?1", [id.as_str()])
            .unwrap();
        let mut state = test_app().await;
        state.storage = storage.clone();
        let response = router::create_router(state)
            .oneshot(Request::post("/v1/forget").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            storage.cleanup_orphan_vectors().await.unwrap(),
            0,
            "the HTTP forgetting path must already have collected the orphan"
        );
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
                        .body(Body::from(
                            r#"{"content":"bucket isolation probe sentence"}"#,
                        ))
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
                    .body(Body::from(r#"{"q":"rust preference","token_budget":100}"#))
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
        assert!(
            items.len() <= 2,
            "near-duplicate must be deduped, got {items:?}"
        );
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
            let mut req = Request::post("/v1/write").header("content-type", "application/json");
            if let Some(ns) = ns {
                req = req.header("x-merkur-namespace", ns);
            }
            let resp = app
                .clone()
                .oneshot(
                    req.body(Body::from(format!(r#"{{"content":"{content}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let get = |uri: &str| {
            let app = app.clone();
            let uri = uri.to_string();
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
            page1["items"][0]["embedding"].is_null()
                || page1["items"][0].get("embedding").is_none(),
            "embeddings never leave the API"
        );

        let beta = get("/v1/memories?namespace=beta").await;
        assert_eq!(beta["total"], 1);
        assert_eq!(beta["items"][0]["content"], "beta fact");

        let status = get("/v1/status").await;
        assert_eq!(status["by_namespace"]["default"], 3);
        assert_eq!(status["by_namespace"]["beta"], 1);
    }

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
}
