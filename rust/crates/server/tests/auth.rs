//! Phase 2: first-run setup, sign-in (lockable, two-factor), sign-out, password reset,
//! CSRF, and API token authentication, against the Rails schema.

use std::path::PathBuf;

use axum::{
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loco_rails_compat::devise::{TokenGenerator, Totp};
use loco_server::{AppState, config::Config, router};
use sqlx::PgPool;
use tower::ServiceExt;

const SECRET_KEY_BASE: &str = "test-secret-key-base";
/// `$2a$10$` hash of this password, produced by Rails (rails-compat golden data).
const RAILS_PASSWORD: &str = "correct horse battery staple";
const RAILS_HASH: &str = "$2a$10$m1sgJU6Y01Iybrg3fA2T1er9GZ5iS3D5F9YVAxin0rHeVr2CraJVi";

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url: String::new(),
        database_pool_size: 1,
        secret_key_base: SECRET_KEY_BASE.into(),
        encryption_secret: None,
        workdir: tempfile::tempdir().unwrap().keep(),
        app_url: None,
        app_root: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../..")),
        production: false,
        job_concurrency: 0,
        force_ssl: false,
    }
}

struct Client {
    app: axum::Router,
    state: AppState,
    cookie: Option<String>,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .map_or("", |v| v.to_str().unwrap())
    }

    /// The first `authenticity_token` on the page.
    fn csrf(&self) -> String {
        const NEEDLE: &str = "name=\"csrf-token\" content=\"";
        let start = self.body.find(NEEDLE).expect("csrf meta tag") + NEEDLE.len();
        let end = self.body[start..].find('"').unwrap();
        self.body[start..start + end].to_owned()
    }
}

impl Client {
    async fn new(db: PgPool) -> Self {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        loco_db::migrate(&db).await.unwrap();
        let state = AppState::build(test_config(), db).await.unwrap();
        Self {
            app: router(state.clone()),
            state,
            cookie: None,
        }
    }

    async fn request(&mut self, request: Request<Body>) -> Reply {
        let mut request = request;
        if let Some(cookie) = &self.cookie {
            request
                .headers_mut()
                .insert(header::COOKIE, cookie.parse().unwrap());
        }
        let response = self.app.clone().oneshot(request).await.unwrap();

        for value in response.headers().get_all(header::SET_COOKIE) {
            let pair = value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            self.cookie = if pair.ends_with('=') {
                None
            } else {
                Some(pair)
            };
        }

        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn get(&mut self, uri: &str) -> Reply {
        self.request(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    async fn form(&mut self, method: Method, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        self.request(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    /// Loads `page` for a CSRF token, then posts the form with it.
    async fn submit(&mut self, page: &str, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let token = self.get(page).await.csrf();
        let mut fields = fields.to_vec();
        fields.push(("authenticity_token", &token));
        self.form(Method::POST, uri, &fields).await
    }
}

async fn insert_account(db: &PgPool, locale: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO accounts (name, locale, timezone, uuid, created_at, updated_at)
         VALUES ('Loco', $1, 'Berlin', gen_random_uuid()::text, now(), now()) RETURNING id",
    )
    .bind(locale)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn insert_user(db: &PgPool, account_id: i64, email: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (account_id, first_name, last_name, email, role, encrypted_password,
                            uuid, created_at, updated_at)
         VALUES ($1, 'Ana', 'Dev', $2, 'admin', $3, gen_random_uuid()::text, now(), now())
         RETURNING id",
    )
    .bind(account_id)
    .bind(email)
    .bind(RAILS_HASH)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn sign_in(client: &mut Client, email: &str, password: &str) -> Reply {
    client
        .submit(
            "/sign_in",
            "/sign_in",
            &[("user[email]", email), ("user[password]", password)],
        )
        .await
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn first_run_setup_creates_the_account_and_signs_in(db: PgPool) {
    let mut client = Client::new(db.clone()).await;

    let home = client.get("/").await;
    assert_eq!(home.status, StatusCode::FOUND);
    assert_eq!(home.location(), "/setup");
    assert_eq!(client.get("/sign_in").await.location(), "/setup");

    let page = client.get("/setup").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Initial Setup"));
    assert!(
        page.body.contains(
            r#"<input type="hidden" value="UTC" name="account[timezone]" id="account_timezone" />"#
        ),
        "{}",
        page.body
    );

    let invalid = client
        .submit(
            "/setup",
            "/setup",
            &[
                ("user[first_name]", "Ana"),
                ("user[email]", "ana@example.test"),
                ("user[password]", "pass"),
                ("encrypted_config[value]", "http://sign.test"),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(invalid.body.contains(r#"value="Ana""#));

    let created = client
        .submit(
            "/setup",
            "/setup",
            &[
                ("user[first_name]", "Ana"),
                ("user[last_name]", "Dev"),
                ("user[email]", " Ana@Example.test"),
                ("user[password]", "password123"),
                ("account[name]", "Loco Lokal"),
                ("account[timezone]", "Europe/Berlin"),
                ("account[locale]", "de-DE"),
                ("encrypted_config[value]", "https://sign.test:8443/"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::FOUND, "{}", created.body);
    assert_eq!(created.location(), "/");

    let (timezone, locale): (String, String) =
        sqlx::query_as("SELECT timezone, locale FROM accounts")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((timezone.as_str(), locale.as_str()), ("Berlin", "de-DE"));

    let (email, sign_in_count): (String, i32) =
        sqlx::query_as("SELECT email, sign_in_count FROM users")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((email.as_str(), sign_in_count), ("ana@example.test", 1));

    let configs: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM encrypted_configs ORDER BY key")
            .fetch_all(&db)
            .await
            .unwrap();
    let decrypt = |v: &str| client.state.secrets.encryptor.decrypt(v).unwrap();
    assert_eq!(configs[0].0, "app_url");
    assert_eq!(decrypt(&configs[0].1), r#""https://sign.test:8443/""#);
    assert_eq!(configs[1].0, "esign_certs");
    let certs: serde_json::Value = serde_json::from_str(&decrypt(&configs[1].1)).unwrap();
    assert!(
        certs["key"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN RSA PRIVATE KEY-----")
    );
    assert_eq!(client.state.default_url(), "https://sign.test:8443");

    let fulltext: String =
        sqlx::query_scalar("SELECT value FROM account_configs WHERE key = 'fulltext_search'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(fulltext, "true");

    let dashboard = client.get("/").await;
    assert_eq!(dashboard.status, StatusCode::OK);
    assert!(
        dashboard
            .body
            .contains(r#"<html data-theme="docuseal" lang="de-DE">"#)
    );
    assert!(
        dashboard
            .body
            .contains(r#"<span class="text-sm align-text-top">AD</span>"#)
    );

    // Setup is closed once a user exists.
    assert_eq!(client.get("/setup").await.location(), "/");
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn signs_in_with_a_rails_password_and_out_again(db: PgPool) {
    let account_id = insert_account(&db, "en-US").await;
    let user_id = insert_user(&db, account_id, "ana@example.test").await;
    let mut client = Client::new(db.clone()).await;

    let redirect = client.get("/").await;
    assert_eq!(redirect.location(), "http://localhost:3000/sign_in");
    let page = client.get("/sign_in").await;
    assert!(
        page.body
            .contains("You need to sign in or sign up before continuing.")
    );

    let wrong = sign_in(&mut client, "ana@example.test", "nope").await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(wrong.body.contains("Invalid Email or password."));
    assert!(wrong.body.contains(r#"value="ana@example.test""#));

    let unknown = sign_in(&mut client, "nobody@example.test", RAILS_PASSWORD).await;
    assert!(unknown.body.contains("Invalid Email or password."));

    let ok = sign_in(&mut client, "ANA@example.test ", RAILS_PASSWORD).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(ok.location(), "/");

    let (failed_attempts, ip_count): (i32, i32) =
        sqlx::query_as("SELECT failed_attempts, sign_in_count FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((failed_attempts, ip_count), (0, 1));

    let dashboard = client.get("/").await;
    assert_eq!(dashboard.status, StatusCode::OK);
    assert!(dashboard.body.contains("Signed in successfully."));
    assert_eq!(client.get("/sign_in").await.location(), "/");

    let user = client.get("/api/user").await;
    assert_eq!(
        user.body,
        format!(
            r#"{{"id":{user_id},"first_name":"Ana","last_name":"Dev","email":"ana@example.test"}}"#
        )
    );

    // Sign-out through the navbar's `button_to ... method: :delete` form.
    let token = dashboard.csrf();
    let out = client
        .form(
            Method::POST,
            "/sign_out",
            &[("_method", "delete"), ("authenticity_token", &token)],
        )
        .await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rust_sessions WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    assert_eq!(
        client.get("/").await.location(),
        "http://localhost:3000/sign_in"
    );
    assert_eq!(
        client.get("/api/user").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn returns_to_the_requested_page_after_sign_in(db: PgPool) {
    let account_id = insert_account(&db, "en-US").await;
    insert_user(&db, account_id, "ana@example.test").await;
    let mut client = Client::new(db).await;

    client.get("/up?x=1").await;
    assert_eq!(
        client.get("/?tab=archived").await.location(),
        "http://localhost:3000/sign_in"
    );
    let ok = sign_in(&mut client, "ana@example.test", RAILS_PASSWORD).await;
    assert_eq!(ok.location(), "/?tab=archived");
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn rejects_requests_without_a_valid_authenticity_token(db: PgPool) {
    let account_id = insert_account(&db, "en-US").await;
    insert_user(&db, account_id, "ana@example.test").await;
    let mut client = Client::new(db).await;

    let token = client.get("/sign_in").await.csrf();
    let fields = [
        ("user[email]", "ana@example.test"),
        ("user[password]", RAILS_PASSWORD),
    ];

    assert_eq!(
        client.form(Method::POST, "/sign_in", &fields).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut forged = fields.to_vec();
    forged.push(("authenticity_token", "Zm9yZ2Vk"));
    assert_eq!(
        client.form(Method::POST, "/sign_in", &forged).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // The X-CSRF-Token header (Turbo) and a matching Origin pass.
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    let reply = client
        .request(
            Request::post("/sign_in")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("x-csrf-token", &token)
                .header(header::HOST, "sign.test")
                .header(header::ORIGIN, "http://sign.test")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    let token = client.get("/").await.csrf();
    let cross_site = client
        .request(
            Request::post("/sign_out")
                .header("x-csrf-token", &token)
                .header(header::HOST, "sign.test")
                .header(header::ORIGIN, "https://evil.test")
                .header("x-http-method-override", "DELETE")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(cross_site.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn locks_the_account_after_too_many_failed_attempts(db: PgPool) {
    let account_id = insert_account(&db, "en-US").await;
    let user_id = insert_user(&db, account_id, "ana@example.test").await;
    sqlx::query("UPDATE users SET failed_attempts = 14 WHERE id = $1")
        .bind(user_id)
        .execute(&db)
        .await
        .unwrap();
    let mut client = Client::new(db.clone()).await;

    let locked = sign_in(&mut client, "ana@example.test", "nope").await;
    assert!(
        locked.body.contains("Your account is locked."),
        "{}",
        locked.body
    );
    // Even the right password is refused while locked.
    let still = sign_in(&mut client, "ana@example.test", RAILS_PASSWORD).await;
    assert!(still.body.contains("Your account is locked."));

    // `unlock_in` (23 hours) later, the lock has expired.
    sqlx::query("UPDATE users SET locked_at = now() - interval '24 hours' WHERE id = $1")
        .bind(user_id)
        .execute(&db)
        .await
        .unwrap();
    let ok = sign_in(&mut client, "ana@example.test", RAILS_PASSWORD).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    let (failed, locked_at): (i32, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT failed_attempts, locked_at FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((failed, locked_at), (0, None));
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn requires_and_consumes_the_two_factor_code(db: PgPool) {
    let account_id = insert_account(&db, "en-US").await;
    let user_id = insert_user(&db, account_id, "ana@example.test").await;
    let mut client = Client::new(db.clone()).await;

    let secret = Totp::generate_secret();
    sqlx::query("UPDATE users SET otp_secret = $2, otp_required_for_login = true WHERE id = $1")
        .bind(user_id)
        .bind(client.state.secrets.encryptor.encrypt(&secret))
        .execute(&db)
        .await
        .unwrap();

    let otp_page = sign_in(&mut client, "ana@example.test", RAILS_PASSWORD).await;
    assert_eq!(otp_page.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(otp_page.body.contains(r#"name="user[otp_attempt]""#));

    let code = Totp::new(&secret)
        .unwrap()
        .at(chrono::Utc::now().timestamp());
    let wrong_code = if code == "000000" { "111111" } else { "000000" };
    let fields = |code: &'static str| {
        vec![
            ("user[email]", "ana@example.test"),
            ("user[password]", RAILS_PASSWORD),
            ("user[otp_attempt]", code),
        ]
    };

    let wrong = client
        .submit("/sign_in", "/sign_in", &fields(wrong_code))
        .await;
    assert!(wrong.body.contains("Invalid Email or password."));

    let code: &'static str = Box::leak(code.into_boxed_str());
    let ok = client.submit("/sign_in", "/sign_in", &fields(code)).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);

    // A code works once.
    let mut other = Client::new(db.clone()).await;
    let replay = other.submit("/sign_in", "/sign_in", &fields(code)).await;
    assert_eq!(replay.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn resets_a_password_with_an_emailed_token(db: PgPool) {
    let account_id = insert_account(&db, "en-US").await;
    let user_id = insert_user(&db, account_id, "ana@example.test").await;
    let mut client = Client::new(db.clone()).await;

    let sent = client
        .submit(
            "/password/new",
            "/password",
            &[("user[email]", "ana@example.test")],
        )
        .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER);
    assert_eq!(sent.location(), "/sign_in");
    assert!(
        client
            .get("/sign_in")
            .await
            .body
            .contains("You will receive an email with instructions on how to reset your password")
    );
    let stored: Option<String> =
        sqlx::query_scalar("SELECT reset_password_token FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(stored.is_some());

    // Unknown emails get the same answer.
    let unknown = client
        .submit(
            "/password/new",
            "/password",
            &[("user[email]", "x@example.test")],
        )
        .await;
    assert_eq!(unknown.location(), "/sign_in");

    // A token stored the way Devise stores it (as Rails would have emailed it).
    let raw = "rawResetToken1234567";
    let digest = TokenGenerator::new(SECRET_KEY_BASE).digest("reset_password_token", raw);
    sqlx::query("UPDATE users SET reset_password_token = $2, reset_password_sent_at = now() AT TIME ZONE 'UTC' WHERE id = $1")
        .bind(user_id)
        .bind(&digest)
        .execute(&db)
        .await
        .unwrap();

    let edit = client
        .get(&format!("/password/edit?reset_password_token={raw}"))
        .await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains(&format!(r#"value="{raw}""#)));
    assert_eq!(client.get("/password/edit").await.location(), "/sign_in");

    let page = format!("/password/edit?reset_password_token={raw}");
    let mismatch = client
        .submit(
            &page,
            "/password",
            &[
                ("_method", "put"),
                ("user[reset_password_token]", raw),
                ("user[password]", "new-password"),
                ("user[password_confirmation]", "other"),
            ],
        )
        .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        mismatch
            .body
            .contains("Password confirmation doesn&#39;t match Password"),
        "{}",
        mismatch.body
    );
    assert!(
        mismatch
            .body
            .contains("1 error prohibited this user from being saved:")
    );

    let invalid = client
        .submit(
            &page,
            "/password",
            &[
                ("_method", "put"),
                ("user[reset_password_token]", "wrong"),
                ("user[password]", "new-password"),
            ],
        )
        .await;
    assert!(invalid.body.contains("Reset password token is invalid"));

    let reset = client
        .submit(
            &page,
            "/password",
            &[
                ("_method", "put"),
                ("user[reset_password_token]", raw),
                ("user[password]", "new-password"),
                ("user[password_confirmation]", "new-password"),
            ],
        )
        .await;
    assert_eq!(reset.status, StatusCode::SEE_OTHER);
    assert_eq!(client.get("/sign_in").await.location(), "/");

    let (hash, token): (String, Option<String>) =
        sqlx::query_as("SELECT encrypted_password, reset_password_token FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(token.is_none());
    assert!(loco_rails_compat::password::verify(
        "new-password",
        &hash,
        None
    ));
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn authenticates_api_requests_with_access_tokens(db: PgPool) {
    use sha2::{Digest, Sha256};

    let account_id = insert_account(&db, "en-US").await;
    let user_id = insert_user(&db, account_id, "ana@example.test").await;
    let token = "api-token-from-the-settings-page";
    let mut client = Client::new(db.clone()).await;

    sqlx::query(
        "INSERT INTO access_tokens (user_id, token, sha256, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(user_id)
    .bind(client.state.secrets.encryptor.encrypt(token))
    .bind(hex::encode(Sha256::digest(token)))
    .execute(&db)
    .await
    .unwrap();

    let api = |token: &str| {
        Request::get("/api/user")
            .header("x-auth-token", token)
            .body(Body::empty())
            .unwrap()
    };
    let ok = client.request(api(token)).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert!(ok.body.contains(r#""email":"ana@example.test""#));

    let bad = client.request(api("nope")).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad.body, r#"{"error":"Not authenticated"}"#);

    sqlx::query("UPDATE users SET archived_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        client.request(api(token)).await.status,
        StatusCode::UNAUTHORIZED
    );
}
