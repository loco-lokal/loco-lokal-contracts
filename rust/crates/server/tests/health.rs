use axum::{body::Body, http::Request};
use loco_server::{AppState, router};
use tower::ServiceExt;

/// Runs against the migrated dev database (`DATABASE_URL`).
#[tokio::test]
async fn up_returns_ok_when_database_is_reachable() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the dev database");
    let db = loco_db::connect(&url, 1).await.unwrap();

    let response = router(AppState { db })
        .oneshot(Request::get("/up").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}
