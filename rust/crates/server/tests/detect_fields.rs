//! `POST /templates/:id/detect_fields`: the server-sent event stream of detected fields.
//! (`rust/scripts/parity/detect_fields.py` compares the streams with Rails'.)

use std::path::PathBuf;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loco_server::{AppState, config::Config, router};
use loco_storage::Attachment;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

/// `$2a$10$` hash of this password, produced by Rails (rails-compat golden data).
const PASSWORD: &str = "correct horse battery staple";
const HASH: &str = "$2a$10$m1sgJU6Y01Iybrg3fA2T1er9GZ5iS3D5F9YVAxin0rHeVr2CraJVi";

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

struct Reply {
    status: StatusCode,
    content_type: String,
    cookie: Option<String>,
    body: String,
}

async fn send(app: &axum::Router, cookie: Option<&str>, request: Request<Body>) -> Reply {
    let mut request = request;
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        cookie,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn csrf(body: &str) -> String {
    const NEEDLE: &str = "name=\"csrf-token\" content=\"";
    let start = body.find(NEEDLE).expect("csrf meta tag") + NEEDLE.len();
    let end = body[start..].find('"').unwrap();
    body[start..start + end].to_owned()
}

/// A signed-out session: its cookie and a CSRF token from the sign-in page.
async fn signed_out(app: &axum::Router) -> (String, String) {
    let page = send(
        app,
        None,
        Request::get("/sign_in").body(Body::empty()).unwrap(),
    )
    .await;
    (page.cookie.unwrap(), csrf(&page.body))
}

/// Signs in and returns the session cookie and a CSRF token for it.
async fn sign_in(app: &axum::Router, email: &str) -> (String, String) {
    let (cookie, token) = signed_out(app).await;
    let form = form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("user[email]", email),
            ("user[password]", PASSWORD),
            ("authenticity_token", &token),
        ])
        .finish();
    let signed_in = send(
        app,
        Some(&cookie),
        Request::post("/sign_in")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap(),
    )
    .await;
    assert_eq!(
        signed_in.status,
        StatusCode::SEE_OTHER,
        "{}",
        signed_in.body
    );
    let cookie = signed_in.cookie.unwrap();
    let dashboard = send(
        app,
        Some(&cookie),
        Request::get("/").body(Body::empty()).unwrap(),
    )
    .await;
    (dashboard.cookie.unwrap_or(cookie), csrf(&dashboard.body))
}

async fn detect(
    app: &axum::Router,
    session: Option<&(String, String)>,
    uri: &str,
    body: &Value,
) -> Reply {
    let mut request = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some((_, csrf)) = session {
        request = request.header("X-CSRF-Token", csrf);
    }
    send(
        app,
        session.map(|(cookie, _)| cookie.as_str()),
        request.body(Body::from(body.to_string())).unwrap(),
    )
    .await
}

fn events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| serde_json::from_str(chunk.strip_prefix("data: ").unwrap()).unwrap())
        .collect()
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn streams_detected_fields_per_page(db: PgPool) {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    loco_db::migrate(&db).await.unwrap();
    let account_id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (name, locale, timezone, uuid, created_at, updated_at)
         VALUES ('Loco', 'en-US', 'UTC', gen_random_uuid()::text, now(), now()) RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (account_id, first_name, last_name, email, role, encrypted_password,
                            uuid, created_at, updated_at)
         VALUES ($1, 'Ana', 'Dev', 'ana@example.test', 'admin', $2, gen_random_uuid()::text,
                 now(), now())
         RETURNING id",
    )
    .bind(account_id)
    .bind(HASH)
    .fetch_one(&db)
    .await
    .unwrap();
    let folder_id: i64 = sqlx::query_scalar(
        "INSERT INTO template_folders (name, author_id, account_id, created_at, updated_at)
         VALUES ('Default', $1, $2, now(), now()) RETURNING id",
    )
    .bind(user_id)
    .bind(account_id)
    .fetch_one(&db)
    .await
    .unwrap();

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
    let template_id: i64 = sqlx::query_scalar(
        "INSERT INTO templates (slug, name, schema, fields, submitters, author_id, account_id,
                                source, folder_id, preferences, created_at, updated_at)
         VALUES ('detect-slug', 'Detect', '[]', '[]', '[]', $1, $2, 'native', $3, '{}',
                 now(), now())
         RETURNING id",
    )
    .bind(user_id)
    .bind(account_id)
    .bind(folder_id)
    .fetch_one(&db)
    .await
    .unwrap();
    let document = Attachment::create(&db, "Template", template_id, "documents", &blob)
        .await
        .unwrap();
    sqlx::query("UPDATE templates SET schema = $1 WHERE id = $2")
        .bind(json!([{ "attachment_uuid": document.uuid, "name": "sample-document" }]).to_string())
        .bind(template_id)
        .execute(&db)
        .await
        .unwrap();
    let app = router(state);
    let uri = format!("/templates/{template_id}/detect_fields");

    // Devise can't redirect from an `ActionController::Live` thread: Rails answers 500.
    let anonymous = signed_out(&app).await;
    let reply = detect(&app, Some(&anonymous), &uri, &json!({})).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);

    let session = sign_in(&app, "ana@example.test").await;

    let reply = detect(&app, Some(&session), &uri, &json!({ "page": 0 })).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type, "text/event-stream");
    let streamed = events(&reply.body);
    let [page, completed] = streamed.as_slice() else {
        panic!("expected a page and `completed`: {}", reply.body);
    };
    assert_eq!(completed, &json!({ "completed": true }));
    assert_eq!(page["attachment_uuid"], json!(document.uuid));
    assert_eq!(page["page"], json!(0));
    for field in page["fields"].as_array().unwrap() {
        let area = &field["areas"][0];
        assert_eq!(area["page"], json!(0));
        assert_eq!(area["attachment_uuid"], json!(document.uuid));
        for key in ["x", "y", "w", "h"] {
            let value = area[key].as_f64().unwrap();
            assert!((0.0..=1.0).contains(&value), "{key}: {value}");
        }
        assert_eq!(field["required"], json!(field["type"] == "signature"));
    }

    // Errors end the stream without `completed`, as in Rails.
    let out_of_range = detect(&app, Some(&session), &uri, &json!({ "page": 99 })).await;
    assert_eq!(out_of_range.status, StatusCode::OK);
    assert_eq!(out_of_range.body, "");

    // An unknown document: nothing to detect.
    let unknown = detect(
        &app,
        Some(&session),
        &uri,
        &json!({ "attachment_uuid": "nope" }),
    )
    .await;
    assert_eq!(events(&unknown.body), vec![json!({ "completed": true })]);
}
