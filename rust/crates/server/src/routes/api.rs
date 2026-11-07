//! `Api::UsersController`.

use axum::Json;
use serde_json::{Value, json};

use crate::auth::ApiAuth;

/// `GET /api/user`.
pub async fn user(ApiAuth(auth): ApiAuth) -> Json<Value> {
    let user = auth.user;
    Json(json!({
        "id": user.id,
        "first_name": user.first_name,
        "last_name": user.last_name,
        "email": user.email,
    }))
}
