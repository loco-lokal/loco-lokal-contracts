//! `GET /preview/:signed_key/:page`: lazily rendered JPEG previews of document pages.
//! (`rust/scripts/parity/previews.py` compares them with Rails' byte for byte.)

use std::path::PathBuf;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loco_server::{AppState, config::Config, router};
use loco_storage::Attachment;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url: String::new(),
        database_pool_size: 1,
        secret_key_base: "test-secret-key-base".into(),
        encryption_secret: None,
        workdir: tempfile::tempdir().unwrap().keep(),
        app_url: None,
        app_root: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../..")),
        production: false,
        job_concurrency: 0,
        force_ssl: false,
        pdf_format: String::new(),
        certs: serde_json::Map::new(),
    }
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, String, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header(header::HOST, "localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let header = |name| {
        response
            .headers()
            .get(name)
            .map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let (content_type, location) = (header(header::CONTENT_TYPE), header(header::LOCATION));
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, content_type, location, body.to_vec())
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn renders_stores_and_reuses_jpeg_previews(db: PgPool) {
    let state = AppState::build(test_config(), db.clone()).await.unwrap();
    let pdf = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../spec/fixtures/sample-document.pdf"
    ))
    .unwrap();
    let blob = state
        .storage
        .create_and_upload(
            &db,
            pdf.into(),
            "sample-document.pdf",
            Some("application/pdf"),
            json!({}),
        )
        .await
        .unwrap();
    let document = Attachment::create(&db, "Template", 1, "documents", &blob)
        .await
        .unwrap();
    let key = state.secrets.signed_id_verifier.generate(
        &json!([document.id, document.uuid]),
        Some("attachment"),
        None,
    );
    let app = router(state.clone());

    let (status, _, location, _) = get(&app, &format!("/preview/{key}/0.jpg")).await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(
        location.starts_with("http://localhost:3000/disk/") && location.ends_with("/0.jpg"),
        "{location}"
    );

    let previews = Attachment::with_blobs_for_records(
        &db,
        "ActiveStorage::Attachment",
        &[document.id],
        "preview_images",
    )
    .await
    .unwrap();
    let [(_, preview)] = previews.as_slice() else {
        panic!("expected one preview, got {}", previews.len());
    };
    assert_eq!(preview.filename, "0.jpg");
    assert_eq!(preview.content_type.as_deref(), Some("image/jpeg"));
    let metadata = preview.metadata.as_deref().unwrap();
    assert!(
        metadata.starts_with(r#"{"analyzed":true,"identified":true,"width":1400,"height":"#),
        "{metadata}"
    );
    let jpeg = state.storage.download(preview).await.unwrap();
    assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF, 0xE1]));

    // The stored preview is reused, also through a signed uuid.
    let signed_uuid =
        state
            .secrets
            .signed_id_verifier
            .generate(&json!(document.uuid), Some("attachment"), None);
    let (status, _, again, _) = get(&app, &format!("/preview/{signed_uuid}/0.jpg")).await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(again.ends_with("/0.jpg"), "{again}");
    let count = Attachment::for_record(
        &db,
        "ActiveStorage::Attachment",
        document.id,
        "preview_images",
    )
    .await
    .unwrap()
    .len();
    assert_eq!(count, 1);

    // Pages out of range fail like `Pdfium::PdfiumError` does in Rails.
    let (status, content_type, _, body) = get(&app, &format!("/preview/{key}/9.jpg")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type, "image/jpeg; charset=utf-8");
    assert!(String::from_utf8_lossy(&body).contains("(500)"));
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn unknown_keys_are_not_found(db: PgPool) {
    let state = AppState::build(test_config(), db).await.unwrap();
    let missing = state.secrets.signed_id_verifier.generate(
        &json!([999_999, "no-such-uuid"]),
        Some("attachment"),
        None,
    );
    let app = router(state);

    for (path, expected) in [
        ("/preview/nope/1.jpg".to_owned(), "image/jpeg"),
        ("/preview/nope/1.png".to_owned(), "image/png"),
        ("/preview/nope/1".to_owned(), "text/html"),
        (format!("/preview/{missing}/1.jpg"), "image/jpeg"),
    ] {
        let (status, content_type, _, body) = get(&app, &path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(content_type, expected, "{path}");
        assert!(body.is_empty(), "{path}");
    }
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn builder_pages_need_a_session(db: PgPool) {
    loco_db::migrate(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO accounts (id, name, locale, timezone, uuid, created_at, updated_at)
         VALUES (1, 'Account', 'en-US', 'UTC', 'acc-1', now(), now())",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (account_id, first_name, last_name, email, role, encrypted_password,
                            uuid, created_at, updated_at)
         VALUES (1, 'A', 'B', 'a@example.com', 'admin', 'x', 'user-1', now(), now())",
    )
    .execute(&db)
    .await
    .unwrap();
    let app = router(AppState::build(test_config(), db).await.unwrap());

    for path in [
        "/templates/1/preview",
        "/templates/1/documents_page_objects?attachment_uuid=x&page=0",
    ] {
        let (status, _, location, _) = get(&app, path).await;
        assert_eq!(status, StatusCode::FOUND, "{path}");
        assert_eq!(location, "http://localhost:3000/sign_in", "{path}");
    }
}
