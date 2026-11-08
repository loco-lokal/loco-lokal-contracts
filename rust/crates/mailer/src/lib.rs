//! Outgoing email over SMTP, choosing settings the way production Rails does:
//!
//! 1. `SMTP_ADDRESS` and the other `SMTP_*` variables (`config/environments/production.rb`).
//!    The sender is replaced with `SMTP_FROM` (a comma-separated list, one picked at random).
//! 2. Otherwise the account's `action_mailer_smtp` encrypted config
//!    (`lib/action_mailer_configs_interceptor.rb`), sending as `"<account name>" <from_email>`.
//! 3. Otherwise mail is discarded (Rails' `:test` delivery) and a warning is logged.

pub mod plain_text;

use std::time::Duration;

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Attachment, Mailbox, MultiPart, header::ContentType},
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{Tls, TlsParameters},
    },
};
use rand::seq::IndexedRandom as _;
use serde_json::Value;

/// `ApplicationMailer` default sender.
pub const DEFAULT_FROM: &str = "DocuSeal <info@docuseal.com>";

#[derive(Debug, thiserror::Error)]
pub enum MailerError {
    #[error("invalid address {0:?}")]
    Address(String),
    #[error(transparent)]
    Build(#[from] lettre::error::Error),
    #[error(transparent)]
    Smtp(#[from] lettre::transport::smtp::Error),
    #[error("invalid SMTP configuration: {0}")]
    Config(String),
    #[error("invalid header name {0:?}")]
    InvalidHeader(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Security {
    /// Plain connection upgraded with STARTTLS; `required` fails if the server lacks it.
    StartTls {
        required: bool,
    },
    /// Implicit TLS (usually port 465).
    Tls,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub domain: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// `plain`, `login`, or `cram_md5` (treated as login, which lettre supports).
    pub authentication: Option<String>,
    pub security: Security,
    pub verify_certificates: bool,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    Smtp {
        settings: SmtpSettings,
        /// Replacement sender(s) from `SMTP_FROM`, or the account sender.
        from: SenderOverride,
    },
    Discard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SenderOverride {
    /// `SMTP_FROM` candidates, one picked per message; it replaces the whole sender.
    Env(Vec<String>),
    /// Account SMTP settings: `"<account name>" <from_email>`.
    Fixed(String),
    None,
}

impl Delivery {
    /// SMTP settings from `SMTP_*` environment variables, if `SMTP_ADDRESS` is set.
    pub fn from_env_lookup(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let host = var("SMTP_ADDRESS")?;
        let password = var("SMTP_PASSWORD");
        let security = if var("SMTP_ENABLE_TLS").as_deref() == Some("true")
            || var("SMTP_ENABLE_SSL").as_deref() == Some("true")
        {
            Security::Tls
        } else if var("SMTP_ENABLE_STARTTLS").as_deref() == Some("false") {
            // Net::SMTP still uses STARTTLS opportunistically (enable_starttls_auto).
            Security::StartTls { required: false }
        } else {
            Security::StartTls { required: true }
        };
        let timeout = var("SMTP_READ_TIMEOUT")
            .and_then(|t| t.parse().ok())
            .unwrap_or(25);

        Some(Self::Smtp {
            settings: SmtpSettings {
                host,
                port: var("SMTP_PORT").and_then(|p| p.parse().ok()).unwrap_or(587),
                domain: var("SMTP_DOMAIN"),
                username: var("SMTP_USERNAME"),
                authentication: password
                    .as_ref()
                    .map(|_| var("SMTP_AUTHENTICATION").unwrap_or_else(|| "plain".into())),
                password,
                security,
                verify_certificates: var("SMTP_SSL_VERIFY").as_deref() != Some("false"),
                timeout: Duration::from_secs(timeout),
            },
            from: var("SMTP_FROM").map_or(SenderOverride::None, |from| {
                SenderOverride::Env(
                    from.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            }),
        })
    }

    /// Settings from the decrypted `action_mailer_smtp` encrypted config value.
    pub fn from_account_config(value: &Value, account_name: &str) -> Result<Self, MailerError> {
        let get = |key: &str| {
            value
                .get(key)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .filter(|s| !s.is_empty())
        };

        let host = get("host").ok_or_else(|| MailerError::Config("missing host".into()))?;
        let port: u16 = get("port")
            .map(|p| p.parse())
            .transpose()
            .map_err(|_| MailerError::Config("invalid port".into()))?
            .unwrap_or(587);
        let security = get("security");
        let password = get("password");

        let (security, verify_certificates) = match security.as_deref() {
            Some("tls" | "ssl") => (Security::Tls, true),
            None if port == 465 => (Security::Tls, true),
            Some("noverify") => (Security::StartTls { required: false }, false),
            _ => (Security::StartTls { required: true }, true),
        };

        let from_email =
            get("from_email").ok_or_else(|| MailerError::Config("missing from_email".into()))?;

        Ok(Self::Smtp {
            settings: SmtpSettings {
                host,
                port,
                domain: get("domain"),
                username: get("username"),
                authentication: password
                    .as_ref()
                    .map(|_| get("authentication").unwrap_or_else(|| "plain".into())),
                password,
                security,
                verify_certificates,
                timeout: Duration::from_secs(25),
            },
            from: SenderOverride::Fixed(format!(
                "\"{}\" <{from_email}>",
                account_name.replace('"', "")
            )),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Email {
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    pub html: String,
    /// Plain-text part; generated from `html` when absent (`HtmlToPlainTextInterceptor`).
    pub text: Option<String>,
    pub attachments: Vec<EmailAttachment>,
    /// Extra headers such as `X-Message-Uuid`.
    pub headers: Vec<(String, String)>,
}

/// What a delivery did, for `EmailEvent` records (`ActionMailerEventsObserver`).
#[derive(Clone, Debug)]
pub struct Delivered {
    /// `mail.from`: the sender addresses.
    pub from: Vec<String>,
    /// `mail.to + mail.cc + mail.bcc`: the recipient addresses.
    pub to: Vec<String>,
    /// `mail.delivery_method.class.name.underscore`.
    pub method: &'static str,
}

#[derive(Clone, Debug)]
pub struct EmailAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct Mailer {
    delivery: Delivery,
    transport: Option<AsyncSmtpTransport<Tokio1Executor>>,
}

impl Mailer {
    pub fn new(delivery: Delivery) -> Result<Self, MailerError> {
        let transport = match &delivery {
            Delivery::Smtp { settings, .. } => Some(build_transport(settings)?),
            Delivery::Discard => None,
        };

        Ok(Self {
            delivery,
            transport,
        })
    }

    #[must_use]
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    /// Builds the MIME message with the sender rules applied.
    pub fn build(&self, email: &Email) -> Result<lettre::Message, MailerError> {
        let from = resolve_from(
            email.from.as_deref().unwrap_or(DEFAULT_FROM),
            match &self.delivery {
                Delivery::Smtp { from, .. } => from,
                Delivery::Discard => &SenderOverride::None,
            },
        );

        let mut builder = lettre::Message::builder()
            .from(parse_mailbox(&from)?)
            .subject(&email.subject);
        // Each entry may itself be a list (`"a@x.test, \"B\" <b@x.test>"`), as Mail
        // accepts for `to:`.
        for to in &email.to {
            for mailbox in parse_mailboxes(to)? {
                builder = builder.to(mailbox);
            }
        }
        for cc in &email.cc {
            for mailbox in parse_mailboxes(cc)? {
                builder = builder.cc(mailbox);
            }
        }
        for bcc in &email.bcc {
            for mailbox in parse_mailboxes(bcc)? {
                builder = builder.bcc(mailbox);
            }
        }
        if let Some(reply_to) = &email.reply_to {
            builder = builder.reply_to(parse_mailbox(reply_to)?);
        }
        for (name, value) in &email.headers {
            let name = lettre::message::header::HeaderName::new_from_ascii(name.clone())
                .map_err(|_| MailerError::InvalidHeader(name.clone()))?;
            builder = builder.raw_header(lettre::message::header::HeaderValue::new(
                name,
                value.clone(),
            ));
        }

        let text = email
            .text
            .clone()
            .unwrap_or_else(|| plain_text::html_to_plain_text(&email.html));
        let body = MultiPart::alternative_plain_html(text, email.html.clone());

        let message = if email.attachments.is_empty() {
            builder.multipart(body)?
        } else {
            let mut mixed = MultiPart::mixed().multipart(body);
            for attachment in &email.attachments {
                let content_type = ContentType::parse(&attachment.content_type)
                    .unwrap_or(ContentType::parse("application/octet-stream").expect("valid"));
                mixed = mixed.singlepart(
                    Attachment::new(attachment.filename.clone())
                        .body(attachment.data.clone(), content_type),
                );
            }
            builder.multipart(mixed)?
        };

        Ok(message)
    }

    pub async fn deliver(&self, email: &Email) -> Result<Delivered, MailerError> {
        let message = self.build(email)?;
        let from = message
            .envelope()
            .from()
            .map(|a| vec![a.to_string()])
            .unwrap_or_default();
        let to = message
            .envelope()
            .to()
            .iter()
            .map(ToString::to_string)
            .collect();

        if let Some(transport) = &self.transport {
            transport.send(message).await?;
            Ok(Delivered {
                from,
                to,
                method: "mail/smtp",
            })
        } else {
            tracing::warn!(
                subject = email.subject,
                "no SMTP configured; email discarded"
            );
            Ok(Delivered {
                from,
                to,
                method: "mail/test_mailer",
            })
        }
    }
}

fn build_transport(
    settings: &SmtpSettings,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, MailerError> {
    let tls_parameters = || {
        TlsParameters::builder(settings.host.clone())
            .dangerous_accept_invalid_certs(!settings.verify_certificates)
            .build()
    };

    let tls = match settings.security {
        Security::Tls => Tls::Wrapper(tls_parameters()?),
        Security::StartTls { required: true } => Tls::Required(tls_parameters()?),
        Security::StartTls { required: false } => Tls::Opportunistic(tls_parameters()?),
        Security::None => Tls::None,
    };

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
        .port(settings.port)
        .tls(tls)
        .timeout(Some(settings.timeout));

    if let Some(domain) = &settings.domain {
        builder = builder.hello_name(lettre::transport::smtp::extension::ClientId::Domain(
            domain.clone(),
        ));
    }

    if let (Some(username), Some(password)) = (&settings.username, &settings.password) {
        let mechanism = match settings.authentication.as_deref() {
            Some("login" | "cram_md5") => Mechanism::Login,
            _ => Mechanism::Plain,
        };
        builder = builder
            .credentials(Credentials::new(username.clone(), password.clone()))
            .authentication(vec![mechanism]);
    }

    Ok(builder.build())
}

/// Applies `SMTP_FROM` / account sender rules to the mailer's `from`.
fn resolve_from(from: &str, rule: &SenderOverride) -> String {
    match rule {
        SenderOverride::None => from.to_owned(),
        SenderOverride::Fixed(sender) => sender.clone(),
        // `ActionMailerConfigsInterceptor`: a bare address replaces the sender. A full
        // `Name <address>` is substituted for the sender's address, which Mail then parses
        // back as that full value, so the configured name replaces the sender's name too.
        SenderOverride::Env(candidates) => candidates
            .choose(&mut rand::rng())
            .cloned()
            .unwrap_or_else(|| from.to_owned()),
    }
}

fn parse_mailboxes(addresses: &str) -> Result<Vec<Mailbox>, MailerError> {
    addresses
        .parse::<lettre::message::Mailboxes>()
        .map(|list| list.into_iter().collect())
        .map_err(|_| MailerError::Address(addresses.to_owned()))
}

fn parse_mailbox(address: &str) -> Result<Mailbox, MailerError> {
    address
        .parse()
        .map_err(|_| MailerError::Address(address.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn env_settings_match_rails_defaults() {
        let delivery = Delivery::from_env_lookup(lookup(&[
            ("SMTP_ADDRESS", "smtp.example.com"),
            ("SMTP_USERNAME", "user"),
            ("SMTP_PASSWORD", "secret"),
            ("SMTP_FROM", "contracts@example.com"),
        ]))
        .unwrap();

        let Delivery::Smtp { settings, from } = delivery else {
            panic!("expected SMTP");
        };
        assert_eq!(settings.port, 587);
        assert_eq!(settings.security, Security::StartTls { required: true });
        assert_eq!(settings.authentication.as_deref(), Some("plain"));
        assert!(settings.verify_certificates);
        assert_eq!(
            from,
            SenderOverride::Env(vec!["contracts@example.com".into()])
        );

        assert!(Delivery::from_env_lookup(lookup(&[])).is_none());
    }

    #[test]
    fn account_config_uses_implicit_tls_on_465() {
        let delivery = Delivery::from_account_config(
            &serde_json::json!({
                "host": "mail.example.com", "port": "465", "username": "u",
                "password": "p", "from_email": "sign@example.com"
            }),
            "Loco \"Lokal\"",
        )
        .unwrap();

        let Delivery::Smtp { settings, from } = delivery else {
            panic!("expected SMTP");
        };
        assert_eq!(settings.security, Security::Tls);
        assert_eq!(
            from,
            SenderOverride::Fixed("\"Loco Lokal\" <sign@example.com>".into())
        );
    }

    #[test]
    fn smtp_from_replaces_sender_like_the_interceptor() {
        let env = |v: &str| SenderOverride::Env(vec![v.into()]);

        assert_eq!(
            resolve_from("Loco Lokal <noreply@x.test>", &env("contracts@example.com")),
            "contracts@example.com"
        );
        assert_eq!(
            resolve_from(
                "Loco Lokal <noreply@x.test>",
                &env("Other <contracts@example.com>")
            ),
            "Other <contracts@example.com>"
        );
    }

    #[test]
    fn builds_multipart_message_with_generated_text() {
        let mailer = Mailer::new(Delivery::Discard).unwrap();
        let message = mailer
            .build(&Email {
                to: vec!["ana@example.com".into()],
                subject: "Bitte unterschreiben".into(),
                html: "<p>Hallo <b>Ana</b></p><p><a href=\"https://x.test/s/abc\">Sign</a></p>"
                    .into(),
                attachments: vec![EmailAttachment {
                    filename: "contract.pdf".into(),
                    content_type: "application/pdf".into(),
                    data: b"%PDF".to_vec(),
                }],
                ..Email::default()
            })
            .unwrap();
        let raw = String::from_utf8(message.formatted()).unwrap();

        assert!(raw.contains("From: DocuSeal <info@docuseal.com>"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("Hallo Ana"));
        assert!(raw.contains("contract.pdf"));
    }
}
