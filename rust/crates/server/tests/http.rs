use std::path::PathBuf;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loco_server::{AppState, config::Config, router};
use sqlx::PgPool;
use tower::ServiceExt;

fn test_config(secret_key_base: &str) -> Config {
    let workdir = tempfile::tempdir().unwrap().keep();
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url: String::new(),
        database_pool_size: 1,
        secret_key_base: secret_key_base.into(),
        encryption_secret: None,
        workdir,
        app_url: None,
        app_root: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../..")),
        production: false,
        job_concurrency: 0,
        force_ssl: false,
    }
}

async fn app(db: PgPool) -> axum::Router {
    router(
        AppState::build(test_config("test-secret-key-base"), db)
            .await
            .unwrap(),
    )
}

async fn get(app: &axum::Router, uri: &str, accept: Option<&str>) -> (StatusCode, String, String) {
    let mut request = Request::get(uri);
    if let Some(accept) = accept {
        request = request.header(header::ACCEPT, accept);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn health_check(db: PgPool) {
    let app = app(db).await;
    assert_eq!(get(&app, "/up", None).await.0, StatusCode::OK);
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn serves_public_files(db: PgPool) {
    let app = app(db).await;
    let (status, _, body) = get(&app, "/robots.txt", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../public/robots.txt"
        ))
        .unwrap()
    );
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn not_found_renders_rails_error_page_or_json(db: PgPool) {
    let app = app(db).await;

    let (status, content_type, body) = get(&app, "/no-such-page", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(content_type.starts_with("text/html"));
    assert_eq!(
        body,
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../app/views/errors/404.html"
        ))
        .unwrap()
    );

    let (status, content_type, body) = get(&app, "/api/nothing", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type, "application/json");
    assert_eq!(body, r#"{"status":404}"#);

    let (_, content_type, _) = get(&app, "/nothing", Some("application/json")).await;
    assert_eq!(content_type, "application/json");
}

/// SMTP settings saved through the Rails settings page are decrypted and used at boot.
#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn loads_rails_encrypted_smtp_settings(db: PgPool) {
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../rails-compat/tests/fixtures/golden.json"
    ))
    .unwrap();

    sqlx::query(
        "INSERT INTO accounts (id, name, locale, timezone, uuid, created_at, updated_at)
         VALUES (1, 'Loco \"Lokal\"', 'de-DE', 'Berlin', 'acc-1', now(), now())",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO encrypted_configs (account_id, key, value, created_at, updated_at)
         VALUES (1, 'action_mailer_smtp', $1, now(), now())",
    )
    .bind(golden["encrypted_config_smtp"]["stored"].as_str().unwrap())
    .execute(&db)
    .await
    .unwrap();

    let config = test_config(golden["secret_key_base"].as_str().unwrap());
    let state = AppState::build(config, db).await.unwrap();

    let loco_mailer::Delivery::Smtp { settings, from } = state.mailer.delivery() else {
        panic!("expected SMTP delivery from the stored settings");
    };
    assert_eq!(settings.host, "mail.example.test");
    assert_eq!(settings.port, 465);
    assert_eq!(settings.security, loco_mailer::Security::Tls);
    assert_eq!(settings.password.as_deref(), Some("smtp-secret"));
    assert_eq!(
        from,
        &loco_mailer::SenderOverride::Fixed("\"Loco Lokal\" <contracts@example.test>".into())
    );
}

/// The submission preview and the phone signature page answer 404 for unknown slugs, as
/// `find_by!` does in Rails.
#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn preview_pages_of_unknown_slugs_are_not_found(db: PgPool) {
    let app = app(db).await;
    for uri in [
        "/e/nope",
        "/e/nope?sig=bogus",
        "/e/nope/completed",
        "/e/nope/download",
        "/p/nope?f=abc",
    ] {
        assert_eq!(get(&app, uri, None).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
}
