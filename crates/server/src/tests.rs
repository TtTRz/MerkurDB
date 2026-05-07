#[cfg(test)]
mod integration {
    use crate::app_state::AppState;
    use crate::router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use merkur_consolidators::NoopConsolidator;
    use merkur_core::{Consolidator, Forgetter};
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
}
