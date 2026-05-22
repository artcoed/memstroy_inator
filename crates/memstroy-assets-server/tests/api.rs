//! Integration tests that drive the assets server in-process via
//! `tower::ServiceExt::oneshot`. We don't bind a real socket — the
//! router is a plain `tower::Service` that we can call directly.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt; // for `.collect()`
use memstroy_assets_server::{router, AssetStore};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt; // for `.oneshot(...)`

fn write(path: &std::path::Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn fixture_store() -> (TempDir, AssetStore) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    write(&root.join("clips/clip_a.mp4"), b"fake mp4 bytes");
    write(&root.join("clips/clip_a.txt"), b"description for clip a");
    write(&root.join("clips/clip_a.tags"), b"funny, cat\nmemes");

    write(&root.join("images/img1.png"), b"\x89PNG\r\n\x1a\n");

    write(&root.join("text/note.md"), b"# hello\nworld");

    let store = AssetStore::new();
    store.index_dir(root).unwrap();
    (tmp, store)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("valid json")
}

#[tokio::test]
async fn health_returns_count() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["ok"], Value::Bool(true));
    assert_eq!(v["count"], Value::from(3));
}

#[tokio::test]
async fn list_returns_total_offset_items() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["total"], Value::from(3));
    assert_eq!(v["offset"], Value::from(0));
    let items = v["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3);

    // Every item must have the required summary fields.
    for item in items {
        assert!(item["id"].is_string());
        assert!(item["kind"].is_string());
        assert!(item["label"].is_string());
        assert!(item["description"].is_string());
        assert!(item["size_bytes"].is_number());
        assert!(item["tags"].is_array());
        // `preview_url` is either a string or null.
        let pu = &item["preview_url"];
        assert!(pu.is_string() || pu.is_null());
    }
}

#[tokio::test]
async fn list_filters_by_kind() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets?kind=image")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["total"], Value::from(1));
    let items = v["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], Value::from("img1"));
    assert_eq!(items[0]["kind"], Value::from("image"));
}

#[tokio::test]
async fn list_search_matches_tags() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets?q=funny")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["total"], Value::from(1));
    assert_eq!(v["items"][0]["id"], Value::from("clip_a"));
}

#[tokio::test]
async fn full_record_endpoint_returns_untruncated_description() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets/clip_a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["id"], Value::from("clip_a"));
    assert_eq!(v["description"], Value::from("description for clip a"));
    assert_eq!(v["tags"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn missing_asset_is_404() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets/does_not_exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
