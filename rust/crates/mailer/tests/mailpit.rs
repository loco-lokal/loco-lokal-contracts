//! Sends a real message through SMTP to Mailpit (compose.rust-dev.yml, CI service).
//! Set `MAILPIT_URL` (e.g. `http://localhost:8025`) and `MAILPIT_SMTP_PORT` (default 1025)
//! to run it; without `MAILPIT_URL` the test is skipped.

use loco_mailer::{Delivery, Email, Mailer, Security, SenderOverride, SmtpSettings};

#[tokio::test]
async fn delivers_to_mailpit() {
    let Ok(api) = std::env::var("MAILPIT_URL") else {
        eprintln!("MAILPIT_URL not set; skipping SMTP delivery test");
        return;
    };
    let port = std::env::var("MAILPIT_SMTP_PORT").map_or(1025, |p| p.parse().unwrap());
    let host = reqwest::Url::parse(&api)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();

    let mailer = Mailer::new(Delivery::Smtp {
        settings: SmtpSettings {
            host,
            port,
            domain: None,
            username: None,
            password: None,
            authentication: None,
            security: Security::None,
            verify_certificates: true,
            timeout: std::time::Duration::from_secs(5),
        },
        from: SenderOverride::Env(vec!["Loco Lokal <contracts@loco-lokal.test>".into()]),
    })
    .unwrap();

    let subject = format!("Rust mailer test {}", std::process::id());
    mailer
        .deliver(&Email {
            to: vec!["signer@example.test".into()],
            subject: subject.clone(),
            html: "<p>Please sign</p>".into(),
            ..Email::default()
        })
        .await
        .unwrap();

    let search: serde_json::Value =
        reqwest::get(format!("{api}/api/v1/search?query=subject:%22{subject}%22"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let message = &search["messages"][0];

    assert_eq!(message["Subject"], subject.as_str());
    assert_eq!(message["From"]["Address"], "contracts@loco-lokal.test");
    assert_eq!(message["To"][0]["Address"], "signer@example.test");
}
