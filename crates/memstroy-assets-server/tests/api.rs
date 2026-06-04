//! Integration tests that drive the assets server in-process via
//! `tower::ServiceExt::oneshot`. We don't bind a real socket — the
//! router is a plain `tower::Service` that we can call directly.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
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
    write(
        &root.join("clips/clip_a.meta.json"),
        br#"{"duration_secs":2.5,"width":1080,"height":1920}"#,
    );

    write(&root.join("images/img1.png"), b"\x89PNG\r\n\x1a\n");

    write(&root.join("text/note.md"), b"# hello\nworld");

    let store = AssetStore::new();
    store.index_dir(root).unwrap();
    (tmp, store)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = body_bytes(resp).await;
    serde_json::from_slice(&bytes).expect("valid json")
}

async fn body_bytes(resp: axum::response::Response) -> bytes::Bytes {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    bytes
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
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=5, stale-while-revalidate=30")
    );
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
        assert!(item["file_name"].is_string());
        assert!(item["extension"].is_string());
        assert!(item["tags"].is_array());
        // `preview_url` is either a string or null.
        let pu = &item["preview_url"];
        assert!(pu.is_string() || pu.is_null());
    }
}

#[tokio::test]
async fn counts_endpoint_returns_totals_by_kind() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets/counts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=5, stale-while-revalidate=30")
    );
    let v = body_json(resp).await;
    assert_eq!(v["total"], Value::from(3));
    assert_eq!(v["counts"]["clip"], Value::from(1));
    assert_eq!(v["counts"]["image"], Value::from(1));
    assert_eq!(v["counts"]["text"], Value::from(1));
    assert_eq!(v["counts"]["video"], Value::from(0));
    let counts = v["counts_by_kind"].as_array().expect("counts array");
    assert!(counts
        .iter()
        .any(|entry| entry["kind"] == Value::from("clip") && entry["count"] == Value::from(1)));
}

#[tokio::test]
async fn list_includes_server_generated_media_metadata() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets?kind=clip&limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let item = &v["items"].as_array().unwrap()[0];
    assert_eq!(item["id"], Value::from("clip_a"));
    assert_eq!(item["file_name"], Value::from("clip_a.mp4"));
    assert_eq!(item["extension"], Value::from("mp4"));
    assert_eq!(item["duration_secs"], Value::from(2.5));
    assert_eq!(item["width"], Value::from(1080));
    assert_eq!(item["height"], Value::from(1920));
    assert_eq!(v["limit"], Value::from(1));
    assert_eq!(v["has_more"], Value::Bool(false));
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
async fn list_search_tolerates_typos() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets?q=funnny")
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
async fn download_supports_byte_ranges() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets/clip_a/download")
                .header("range", "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok()),
        Some("bytes 0-3/14")
    );
    let bytes = body_bytes(resp).await;
    assert_eq!(&bytes[..], b"fake");
}

#[tokio::test]
async fn download_rejects_empty_asset_id_instead_of_placeholder() {
    let (_tmp, store) = fixture_store();
    let app = router(store);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/assets//download")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"], Value::from("bad_request"));
    assert_eq!(v["message"], Value::from("asset id is required"));
}

#[tokio::test]
async fn admin_upload_persists_and_reindexes_asset() {
    std::env::remove_var("ADMIN_TOKEN");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let store = AssetStore::new();
    store.index_dir(root).unwrap();
    let app = router(store.clone());

    let boundary = "memstroy-test-boundary";
    let body = format!(
        concat!(
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"kind\"\r\n\r\n",
            "clip\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"description\"\r\n\r\n",
            "uploaded description\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"tags\"\r\n\r\n",
            "admin,upload\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"asset\"; filename=\"sample.mp4\"\r\n",
            "Content-Type: video/mp4\r\n\r\n",
            "fake mp4 bytes\r\n",
            "--{b}--\r\n"
        ),
        b = boundary
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/assets")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["created"], Value::Bool(true));
    assert_eq!(v["asset"]["id"], Value::from("sample"));

    assert!(root.join("clips/sample.mp4").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("clips/sample.txt")).unwrap(),
        "uploaded description"
    );
    assert!(store.get("sample").is_some());
}

#[tokio::test]
async fn admin_upload_with_explicit_id_replaces_existing_asset() {
    std::env::remove_var("ADMIN_TOKEN");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let store = AssetStore::new();
    store.index_dir(root).unwrap();

    async fn upload(app: axum::Router, id: &str, description: &str, body_bytes: &str) -> Value {
        let boundary = format!("memstroy-test-boundary-{id}");
        let body = format!(
            concat!(
                "--{b}\r\n",
                "Content-Disposition: form-data; name=\"kind\"\r\n\r\n",
                "clip\r\n",
                "--{b}\r\n",
                "Content-Disposition: form-data; name=\"id\"\r\n\r\n",
                "{id}\r\n",
                "--{b}\r\n",
                "Content-Disposition: form-data; name=\"description\"\r\n\r\n",
                "{description}\r\n",
                "--{b}\r\n",
                "Content-Disposition: form-data; name=\"asset\"; filename=\"sample.mp4\"\r\n",
                "Content-Type: video/mp4\r\n\r\n",
                "{body_bytes}\r\n",
                "--{b}--\r\n"
            ),
            b = boundary,
            id = id,
            description = description,
            body_bytes = body_bytes
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/assets")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    }

    let first = upload(
        router(store.clone()),
        "fixed",
        "first description",
        "first bytes",
    )
    .await;
    let second = upload(
        router(store.clone()),
        "fixed",
        "second description",
        "second bytes",
    )
    .await;

    assert_eq!(first["asset"]["id"], Value::from("fixed"));
    assert_eq!(second["asset"]["id"], Value::from("fixed"));
    assert!(root.join("clips/fixed.mp4").exists());
    assert!(!root.join("clips/fixed-1.mp4").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("clips/fixed.txt")).unwrap(),
        "second description"
    );
    let indexed = store.get("fixed").unwrap();
    assert_eq!(indexed.description, "second description");
    assert_eq!(store.count(), 1);
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
