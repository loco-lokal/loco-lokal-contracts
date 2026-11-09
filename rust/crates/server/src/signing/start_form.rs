//! Template share links: `StartFormController` (`/d/:slug`), `StartFormResubmitController`
//! (`/resubmit_form`), `Submitters::StartForm` and `Submissions::AssignDefinedSubmitters`.
//!
//! Email two-factor verification (`shared_link_2fa`, reopening a pending submitter without
//! the start form cookie) answers 422: sending and checking codes is not ported yet.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{Duration, Utc};
use loco_db::{
    JsonText, signing,
    submissions::{self, NewSubmission, NewSubmitter, Submitter},
    templates::{self, Template},
    users::{self, Account},
};
use loco_domain::ability::Action;
use serde_json::{Map, Value, json};

use super::{actions::redirect_to, pages::form_locale};
use crate::{
    api::{Params, create::email::normalize_email, templates::blank},
    auth::{Auth, MaybeAuth},
    errors::AppError,
    page::Page,
    request::ClientIp,
    session::Session,
    state::AppState,
    webhooks::{self, EventRecord},
};

/// `cookies.encrypted[:start_form_slug]`.
pub const START_FORM_COOKIE: &str = "start_form_slug";
/// `cookies.encrypted[:email_2fa_slug]`.
pub const EMAIL_2FA_COOKIE: &str = "email_2fa_slug";
/// `Submitters::StartForm::START_FORM_COOKIES_TTL`.
const START_FORM_COOKIES_TTL: Duration = Duration::days(7);
/// `Submitters::StartForm::RESUBMIT_TTL`.
const RESUBMIT_TTL: Duration = Duration::days(3);
/// `AccountConfig::ALLOW_TO_RESUBMIT`.
const ALLOW_TO_RESUBMIT: &str = "allow_to_resubmit";
/// `AccountConfig::POLICY_LINKS_KEY`.
const POLICY_LINKS_KEY: &str = "policy_links";

fn not_found() -> AppError {
    AppError::Status(StatusCode::NOT_FOUND)
}

fn truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

/// `template.preferences.fetch('link_form_fields', ['email'])`.
fn link_form_fields(template: &Template) -> Vec<String> {
    match template.preferences.get("link_form_fields") {
        None => vec!["email".to_owned()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(_) => Vec::new(),
    }
}

fn template_submitters(template: &Template) -> &[Value] {
    template
        .submitters
        .as_array()
        .map_or(&[][..], Vec::as_slice)
}

/// `Templates.filter_undefined_submitters`.
#[must_use]
pub fn undefined_submitters(submitters: &[Value]) -> Vec<&Value> {
    submitters
        .iter()
        .filter(|s| {
            [
                "invite_by_uuid",
                "optional_invite_by_uuid",
                "invite_via_field_uuid",
                "linked_to_uuid",
                "is_requester",
                "email",
            ]
            .iter()
            .all(|key| s.get(*key).is_none_or(blank))
        })
        .collect()
}

/// `Submitters::StartForm.first_submitter_uuid`.
fn first_submitter_uuid(template: &Template) -> Option<String> {
    let submitters = template_submitters(template);
    undefined_submitters(submitters)
        .first()
        .copied()
        .or_else(|| submitters.first())
        .and_then(|s| s.get("uuid"))
        .map(crate::api::create::values::ruby_to_s)
}

/// `submitter_params`: `params.require(:submitter).permit(:email, :phone, :name)` with the
/// email normalized.
#[derive(Clone, Debug, Default)]
struct Contact {
    email: Option<String>,
    phone: Option<String>,
    name: Option<String>,
}

impl Contact {
    fn from_params(params: &Value) -> Self {
        let get = |key: &str| {
            params.get(key).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
        };
        Self {
            email: get("email").and_then(|e| normalize_email(&e)),
            phone: get("phone"),
            name: get("name"),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "email" => self.email.as_deref(),
            "phone" => self.phone.as_deref(),
            "name" => self.name.as_deref(),
            _ => None,
        }
    }

    fn present(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.trim().is_empty())
    }

    /// `compact_blank.to_query`, keys sorted.
    fn query(&self) -> String {
        let mut pairs = Vec::new();
        for key in ["email", "name", "phone"] {
            if let Some(value) = self.present(key) {
                pairs.push(format!(
                    "{key}={}",
                    percent_encoding::utf8_percent_encode(value, QUERY)
                ));
            }
        }
        pairs.join("&")
    }
}

/// `CGI.escape`.
const QUERY: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'*')
    .remove(b'-')
    .remove(b'.')
    .remove(b'_');

struct Loaded {
    template: Template,
    account: Account,
}

async fn load_template(state: &AppState, slug: &str) -> Result<Loaded, AppError> {
    let template = templates::find_by_slug(&state.db, slug)
        .await?
        .ok_or_else(not_found)?;
    let account = users::find_account(&state.db, template.account_id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Loaded { template, account })
}

async fn can_read(
    state: &AppState,
    auth: Option<&Auth>,
    template: &Template,
) -> Result<bool, AppError> {
    let Some(auth) = auth else {
        return Ok(false);
    };
    let sharings = templates::sharings(&state.db, template.id).await?;
    Ok(auth
        .ability()
        .can_template(Action::Read, Some(template.account_id), &sharings))
}

fn unsupported(
    state: &AppState,
    session: &Session,
    locale: &str,
    template: &Template,
    message: &str,
) -> Result<Response, AppError> {
    Page::new(state, session, locale)
        .status(StatusCode::UNPROCESSABLE_ENTITY)
        .with(json!({ "robots_noindex": true, "name": template.name, "message": message }))
        .render("submit_form/unsupported.html")
}

/// What the start form shows besides the template.
#[derive(Default)]
struct FormState<'a> {
    contact: Contact,
    error_message: Option<String>,
    resubmit_slug: Option<&'a str>,
}

/// Renders `start_form/show`.
#[allow(clippy::too_many_arguments)]
async fn render_show(
    state: &AppState,
    session: &Session,
    auth: Option<&Auth>,
    locale: &str,
    loaded: &Loaded,
    query: &Value,
    form: FormState<'_>,
    status: StatusCode,
) -> Result<Response, AppError> {
    let Loaded { template, account } = loaded;
    let fields = link_form_fields(template);
    let param = |key: &str| query.get(key).and_then(Value::as_str).map(str::to_owned);
    let name_value = auth
        .map(|a| a.user.full_name())
        .or_else(|| param("name"))
        .or_else(|| form.contact.name.clone());
    let email_value = auth
        .map(|a| a.user.email.clone())
        .or_else(|| param("email"))
        .or_else(|| form.contact.email.clone());
    let phone_value = param("phone").or_else(|| form.contact.phone.clone());
    let policy_links_html = signing::account_configs(&state.db, account.id, &[POLICY_LINKS_KEY])
        .await?
        .into_iter()
        .next()
        .map(|(_, v)| crate::markdown::to_html(&crate::api::create::values::ruby_to_s(&v)));
    let description = state.views.t(
        &account.locale,
        "account_name_has_invited_you_to_fill_and_sign_documents_online_effortlessly_with_a_secure_fast_and_user_friendly_digital_document_signing_solution",
        &[("account_name", account.name.clone())],
    );
    Page::new(state, session, locale)
        .auth(auth)
        .status(status)
        .with(json!({
            "robots_noindex": true,
            "html_title": format!("{} | Loco Lokal", template.name),
            "html_description": description,
            "template_name": template.name,
            "template_archived": template.archived_at.is_some(),
            "account_name": account.name,
            "account_archived": account.archived_at.is_some(),
            "form_action": if form.resubmit_slug.is_some() {
                "/resubmit_form".to_owned()
            } else {
                format!("/d/{}", template.slug)
            },
            "resubmit_slug": form.resubmit_slug,
            "error_message": form.error_message,
            "link_form_fields": fields,
            "multiple_fields": fields.len() > 1,
            "name_value": name_value,
            "email_value": email_value,
            "phone_value": phone_value,
            "policy_links_html": policy_links_html,
        }))
        .render("start_form/show.html")
}

/// `GET /d/:slug`.
pub async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let loaded = load_template(&state, &slug).await?;
    let template = &loaded.template;
    if truthy(template.preferences.get("require_phone_2fa"))
        || truthy(template.preferences.get("require_email_2fa"))
    {
        return Err(not_found());
    }
    if template.shared_link {
        if params.str("email_verification").is_some() {
            return unsupported(
                &state,
                &session,
                &locale,
                template,
                "Email two-factor verification is not supported yet",
            );
        }
        return render_show(
            &state,
            &session,
            auth.as_ref(),
            &locale,
            &loaded,
            &params.0,
            FormState::default(),
            StatusCode::OK,
        )
        .await;
    }
    if can_read(&state, auth.as_ref(), template).await? {
        let can_update = auth.as_ref().is_some_and(|a| {
            a.ability()
                .can_template(Action::Update, Some(template.account_id), &[])
        });
        return Page::new(&state, &session, &locale)
            .auth(auth.as_ref())
            .with(json!({
                "robots_noindex": true,
            "html_title": format!("{} | Loco Lokal", template.name),
                "html_description": state.views.t(
                    &loaded.account.locale,
                    "share_link_is_currently_disabled",
                    &[],
                ),
                "template_id": template.id,
                "template_slug": template.slug,
                "template_name": template.name,
                "template_archived": template.archived_at.is_some(),
                "account_name": loaded.account.name,
                "can_update": can_update,
            }))
            .render("start_form/private.html");
    }
    Err(not_found())
}

/// `Submitters::StartForm.can_reopen?`.
async fn can_reopen(
    state: &AppState,
    headers: &HeaderMap,
    auth: Option<&Auth>,
    template: &Template,
    submitter: &Submitter,
) -> Result<bool, AppError> {
    let form = super::Form::for_submitter(state, submitter.clone()).await?;
    if form.submission.completed_at.is_some() && form.is_viewer(submitter) {
        return Ok(false);
    }
    let cookie = |name| crate::cookies::encrypted(state, headers, name);
    if cookie(START_FORM_COOKIE).as_deref() == Some(submitter.slug.as_str())
        || cookie(EMAIL_2FA_COOKIE).as_deref() == Some(submitter.slug.as_str())
    {
        return Ok(true);
    }
    if submitter
        .email
        .as_deref()
        .is_none_or(|e| e.trim().is_empty())
    {
        return Ok(false);
    }
    if submitter.completed_at.is_some() {
        return Ok(true);
    }
    if form.submission.source == "embed" {
        return Ok(false);
    }
    if template.preferences.get("shared_link_2fa") == Some(&Value::Bool(true)) {
        return Ok(true);
    }
    if form.submission.source == "link" {
        return Ok(false);
    }
    if let Some(auth) = auth
        && submitter.email.as_deref() == Some(auth.user.email.as_str())
        && auth.user.account_id == submitter.account_id
    {
        return Ok(true);
    }
    Ok(crate::mail::can_send_emails(state).await?)
}

/// A submitter to create for the start form.
struct NewParty {
    email: Option<String>,
    phone: Option<String>,
    name: Option<String>,
    values: Map<String, Value>,
    preferences: Value,
    metadata: Value,
}

/// `Submitters::StartForm.assign_submission_attributes` +
/// `Submissions::AssignDefinedSubmitters` + `save` + `enqueue_new_submitter_jobs`.
async fn create_submission(
    state: &AppState,
    template: &Template,
    party: NewParty,
    source: &str,
    ip: Option<&str>,
    ua: Option<&str>,
) -> Result<Submitter, AppError> {
    let uuid = first_submitter_uuid(template).unwrap_or_default();
    let mut snapshot = template_submitters(template).to_vec();
    // `assign_submitters_is_viewer`: parties without fields only view.
    let field_uuids: Vec<&Value> = template
        .fields
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .filter_map(|f| f.get("submitter_uuid"))
                .collect()
        })
        .unwrap_or_default();
    for s in &mut snapshot {
        if !s.get("uuid").is_some_and(|u| field_uuids.contains(&u))
            && let Some(map) = s.as_object_mut()
        {
            map.insert("is_viewer".into(), Value::Bool(true));
        }
    }

    // `assign_defined_submitters` and `assign_linked_submitters`.
    let mut others: Vec<(String, String)> = Vec::new();
    let mut author_email = None;
    for item in template_submitters(template) {
        let Some(item_uuid) = item.get("uuid").map(crate::api::create::values::ruby_to_s) else {
            continue;
        };
        let is_requester = item.get("is_requester").is_some_and(|v| !blank(v));
        if item.get("email").is_none_or(blank) && !is_requester {
            continue;
        }
        if item_uuid == uuid || others.iter().any(|(u, _)| *u == item_uuid) {
            continue;
        }
        let email = if is_requester {
            if author_email.is_none() {
                author_email = users::find(&state.db, template.author_id)
                    .await?
                    .map(|u| u.email);
            }
            author_email.clone().unwrap_or_default()
        } else {
            crate::api::create::values::ruby_to_s(item.get("email").unwrap_or(&Value::Null))
        };
        others.push((item_uuid, email));
    }
    for item in template_submitters(template) {
        let Some(linked) = item.get("linked_to_uuid").filter(|v| !blank(v)) else {
            continue;
        };
        let Some(item_uuid) = item.get("uuid").map(crate::api::create::values::ruby_to_s) else {
            continue;
        };
        if item_uuid == uuid || others.iter().any(|(u, _)| *u == item_uuid) {
            continue;
        }
        let linked = crate::api::create::values::ruby_to_s(linked);
        let email = if linked == uuid {
            party.email.clone()
        } else {
            others
                .iter()
                .find(|(u, _)| *u == linked)
                .map(|(_, e)| e.clone())
        };
        if let Some(email) = email {
            others.push((item_uuid, email));
        }
    }

    let mut tx = state.db.begin().await?;
    let submission = submissions::insert(
        &mut *tx,
        &NewSubmission {
            template_id: Some(template.id),
            created_by_user_id: None,
            account_id: template.account_id,
            slug: loco_rails_compat::base58(14),
            source: source.to_owned(),
            submitters_order: "preserved".to_owned(),
            name: None,
            preferences: JsonText(json!({})),
            variables: None,
            expire_at: crate::api::create::default_expire_at(template),
            template_fields: None,
            template_schema: None,
            template_submitters: Some(JsonText(Value::Array(snapshot))),
            variables_schema: None,
        },
    )
    .await?;
    let submitter = submissions::insert_submitter(
        &mut *tx,
        &NewSubmitter {
            submission_id: submission.id,
            template_id: Some(template.id),
            account_id: template.account_id,
            uuid,
            slug: loco_rails_compat::base58(14),
            email: party.email,
            phone: party.phone,
            name: party.name,
            external_id: None,
            values: JsonText(Value::Object(party.values)),
            metadata: JsonText(party.metadata),
            preferences: JsonText(party.preferences),
            sent_at: None,
        },
    )
    .await?;
    signing::set_submitter_request(&mut *tx, submitter.id, ip, ua).await?;
    for (other_uuid, email) in others {
        submissions::insert_submitter(
            &mut *tx,
            &NewSubmitter {
                submission_id: submission.id,
                template_id: Some(template.id),
                account_id: template.account_id,
                uuid: other_uuid,
                slug: loco_rails_compat::base58(14),
                email: Some(email),
                phone: None,
                name: None,
                external_id: None,
                values: JsonText(json!({})),
                metadata: JsonText(json!({})),
                preferences: JsonText(json!({})),
                sent_at: None,
            },
        )
        .await?;
    }
    tx.commit().await?;

    webhooks::enqueue(
        state,
        "submission.created",
        &[EventRecord {
            id: submission.id,
            account_id: submission.account_id,
        }],
    )
    .await;
    crate::search::reindex_later(state, "Submitter", &[submitter.id]).await;
    if submission.expire_at.is_some() {
        // `ProcessSubmissionExpiredJob` comes with the expiration jobs (Phase 7).
        tracing::debug!(submission_id = submission.id, "submission expires");
    }
    Ok(submissions::find_submitter(&state.db, submitter.id)
        .await?
        .unwrap_or(submitter))
}

/// `Submissions::CreateFromSubmitters.maybe_set_dynamic_documents` would rebuild the
/// documents.
fn has_dynamic_documents(template: &Template) -> bool {
    template.variables_schema.as_ref().is_none_or(|v| blank(v))
        && template
            .schema
            .as_array()
            .is_some_and(|docs| docs.iter().any(|d| truthy(d.get("dynamic"))))
}

/// `multiple_submitters_error_message`.
fn multiple_submitters_error(
    state: &AppState,
    locale: &str,
    auth: Option<&Auth>,
    template: &Template,
) -> String {
    if auth.is_some_and(|a| a.user.account_id == template.account_id) {
        strip_links(&state.views.t(
            locale,
            "this_submission_has_multiple_signers_which_prevents_the_use_of_a_sharing_link_html",
            &[],
        ))
    } else {
        state.views.t(locale, "not_found", &[])
    }
}

/// `strip_links`: drops `<a>` tags, keeping their text.
fn strip_links(html: &str) -> String {
    let re = regex::Regex::new(r"(?i)</?a\b[^>]*>").expect("valid regex");
    re.replace_all(html, "").into_owned()
}

fn start_redirect(state: &AppState, headers: &HeaderMap, slug: &str) -> Response {
    let mut response = redirect_to(state, headers, &format!("/s/{slug}"));
    crate::cookies::set_encrypted(
        state,
        headers,
        &mut response,
        START_FORM_COOKIE,
        slug,
        Utc::now() + START_FORM_COOKIES_TTL,
    );
    response
}

/// `PUT /d/:slug`.
#[allow(clippy::too_many_lines)]
pub async fn update(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let loaded = load_template(&state, &slug).await?;
    let template = &loaded.template;

    // `authorize_start!`
    if template.archived_at.is_some()
        || loaded.account.archived_at.is_some()
        || !(template.shared_link || can_read(&state, auth.as_ref(), template).await?)
    {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/d/{}", template.slug),
        ));
    }

    let Some(submitter_params) = params.get("submitter").filter(|v| !blank(v)) else {
        return Err(AppError::Status(StatusCode::BAD_REQUEST));
    };
    let contact = Contact::from_params(submitter_params);
    let fields = link_form_fields(template);
    if fields.iter().any(|key| contact.present(key).is_none()) {
        return unsupported(
            &state,
            &session,
            &locale,
            template,
            "Starting without the requested details is not supported yet",
        );
    }

    // `Submitters::StartForm.find_or_initialize_submitter`
    let email = fields
        .iter()
        .any(|k| k == "email")
        .then(|| contact.email.clone())
        .flatten();
    let phone = fields
        .iter()
        .any(|k| k == "phone")
        .then(|| contact.phone.clone())
        .flatten();
    let mut existing = if email.is_none() && phone.is_none() {
        None
    } else {
        signing::find_start_form_submitter(
            &state.db,
            template.id,
            email.as_deref(),
            phone.as_deref(),
            params.get("resubmit").is_some_and(|v| !blank(v)),
        )
        .await?
    };
    if let Some(submitter) = &existing
        && !can_reopen(&state, &headers, auth.as_ref(), template, submitter).await?
    {
        existing = None;
    }

    if let Some(submitter) = &existing
        && submitter.completed_at.is_some()
    {
        let query = contact.query();
        let path = if query.is_empty() {
            format!("/d/{}/completed", template.slug)
        } else {
            format!("/d/{}/completed?{query}", template.slug)
        };
        return Ok(redirect_to(&state, &headers, &path));
    }

    if existing.is_none() && undefined_submitters(template_submitters(template)).len() > 1 {
        let error = multiple_submitters_error(&state, &locale, auth.as_ref(), template);
        return render_show(
            &state,
            &session,
            auth.as_ref(),
            &locale,
            &loaded,
            &params.0,
            FormState {
                contact: Contact {
                    email: email.clone(),
                    phone: phone.clone(),
                    name: fields
                        .iter()
                        .any(|k| k == "name")
                        .then(|| contact.name.clone())
                        .flatten(),
                },
                error_message: Some(error),
                resubmit_slug: None,
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }

    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // `require_link_2fa?`
    let shared_link_2fa = template.preferences.get("shared_link_2fa") == Some(&Value::Bool(true));
    let require_2fa = shared_link_2fa
        || existing.as_ref().is_some_and(|s| {
            crate::cookies::encrypted(&state, &headers, START_FORM_COOKIE).as_deref()
                != Some(s.slug.as_str())
                && !auth.as_ref().is_some_and(|a| {
                    s.email.as_deref() == Some(a.user.email.as_str())
                        && a.user.account_id == s.account_id
                })
        });
    if require_2fa {
        // `verify_2fa_and_save_submitter` without a code passes only with the cookie.
        let verified = existing.as_ref().is_some_and(|s| {
            crate::cookies::encrypted(&state, &headers, EMAIL_2FA_COOKIE).as_deref()
                == Some(s.slug.as_str())
        });
        if !verified || params.str("one_time_code").is_some() {
            return unsupported(
                &state,
                &session,
                &locale,
                template,
                "Email two-factor verification is not supported yet",
            );
        }
    }

    let submitter = if let Some(submitter) = existing {
        signing::set_submitter_request(&state.db, submitter.id, ip.as_deref(), ua.as_deref())
            .await?;
        submitter
    } else {
        if has_dynamic_documents(template) {
            return unsupported(
                &state,
                &session,
                &locale,
                template,
                "Templates with dynamic documents are not supported yet",
            );
        }
        let name = fields
            .iter()
            .any(|k| k == "name")
            .then(|| contact.name.clone())
            .flatten();
        create_submission(
            &state,
            template,
            NewParty {
                email,
                phone,
                name,
                values: Map::new(),
                preferences: json!({ "send_email": true }),
                metadata: json!({}),
            },
            "link",
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?
    };
    Ok(start_redirect(&state, &headers, &submitter.slug))
}

/// `GET /d/:slug/completed`.
pub async fn completed(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let loaded = load_template(&state, &slug).await?;
    let template = &loaded.template;
    if !template.shared_link || template.archived_at.is_some() {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/d/{}", template.slug),
        ));
    }
    let contact = Contact::from_params(&params.0);
    let fields = link_form_fields(template);
    if fields.iter().any(|key| contact.present(key).is_none())
        || fields
            .iter()
            .filter(|k| *k != "name")
            .all(|key| contact.present(key).is_none())
    {
        return Err(not_found());
    }
    let has = |key: &str| fields.iter().any(|k| k == key);
    let submitter = signing::find_completed_start_form_submitter(
        &state.db,
        template.id,
        if has("email") {
            contact.email.as_deref()
        } else {
            None
        },
        if has("phone") {
            contact.phone.as_deref()
        } else {
            None
        },
    )
    .await?
    .ok_or_else(not_found)?;
    let form = super::Form::for_submitter(&state, submitter).await?;
    let with_signature_fields = form.fields().iter().any(|f| {
        f.get("submitter_uuid").and_then(Value::as_str) == Some(form.submitter.uuid.as_str())
            && matches!(
                f.get("type").and_then(Value::as_str),
                Some("signature" | "initials")
            )
    });
    let completed_at = form.submitter.completed_at.unwrap_or_default();
    let time = crate::l10n::local_date(
        &state,
        &locale,
        completed_at,
        &form.account.timezone,
        "long",
    );
    let allow_resubmit = signing::account_configs(&state.db, form.account.id, &[ALLOW_TO_RESUBMIT])
        .await?
        .iter()
        .all(|(_, v)| *v != Value::Bool(false));
    let can_resubmit = undefined_submitters(template_submitters(template)).len() == 1
        && !matches!(form.submission.source.as_str(), "api" | "embed" | "mcp")
        && allow_resubmit;
    // `button_to` sorts its params by name.
    let resubmit_params: Vec<(&str, &str)> = ["email", "name", "phone"]
        .into_iter()
        .filter_map(|k| {
            params
                .get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty())
                .map(|v| (k, v))
        })
        .collect();
    Page::new(&state, &session, &locale)
        .auth(auth.as_ref())
        .with(json!({
            "robots_noindex": true,
            "template_slug": template.slug,
            "template_name": form.template.as_ref().map(|t| t.name.clone()),
            "with_signature_fields": with_signature_fields,
            "signed_text": state.views.t(
                &locale,
                if with_signature_fields { "signed_on_time" } else { "completed_on_time" },
                &[("time", time)],
            ),
            "can_email": crate::mail::can_send_emails(&state).await?,
            "email_param": params.get("email").and_then(Value::as_str),
            "can_resubmit": can_resubmit,
            "resubmit_params": resubmit_params,
        }))
        .render("start_form/completed.html")
}

/// `Submitters::StartForm.can_resubmit?`.
pub async fn can_resubmit(state: &AppState, form: &super::Form) -> Result<bool, AppError> {
    let Some(completed_at) = form.submitter.completed_at else {
        return Ok(false);
    };
    if completed_at <= (Utc::now() - RESUBMIT_TTL).naive_utc()
        || matches!(form.submission.source.as_str(), "api" | "embed" | "mcp")
    {
        return Ok(false);
    }
    Ok(
        signing::account_configs(&state.db, form.account.id, &[ALLOW_TO_RESUBMIT])
            .await?
            .iter()
            .all(|(_, v)| *v != Value::Bool(false)),
    )
}

/// `PUT /resubmit_form`.
pub async fn resubmit(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    // `load_resubmit_submitter`
    let Some(slug) = params.str("resubmit") else {
        return Err(not_found());
    };
    let Some(form) = super::Form::load(&state, &slug).await? else {
        return Err(not_found());
    };
    if !can_resubmit(&state, &form).await? {
        return Err(not_found());
    }
    let Some(template) = form.template.clone() else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "resubmitted submission has no template"
        )));
    };
    let account = users::find_account(&state.db, template.account_id)
        .await?
        .ok_or_else(not_found)?;
    // `authorize_start!`
    if template.archived_at.is_some() || account.archived_at.is_some() {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/s/{}", form.submitter.slug),
        ));
    }
    let resubmitted = &form.submitter;
    let cookie_slug = crate::cookies::encrypted(&state, &headers, START_FORM_COOKIE);
    let existing = signing::find_start_form_submitter_by_slug(
        &state.db,
        template.id,
        cookie_slug.as_deref(),
        resubmitted.email.as_deref(),
    )
    .await?;
    let loaded = Loaded { template, account };
    let template = &loaded.template;

    if existing.is_none() && undefined_submitters(template_submitters(template)).len() > 1 {
        let error = multiple_submitters_error(&state, &locale, auth.as_ref(), template);
        return render_show(
            &state,
            &session,
            auth.as_ref(),
            &locale,
            &loaded,
            &params.0,
            FormState {
                contact: Contact::default(),
                error_message: Some(error),
                resubmit_slug: Some(&resubmitted.slug),
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    if template.preferences.get("shared_link_2fa") == Some(&Value::Bool(true)) {
        return unsupported(
            &state,
            &session,
            &locale,
            template,
            "Email two-factor verification is not supported yet",
        );
    }

    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let submitter = if let Some(submitter) = existing {
        signing::set_submitter_request(&state.db, submitter.id, ip.as_deref(), ua.as_deref())
            .await?;
        submitter
    } else {
        if has_dynamic_documents(template) {
            return unsupported(
                &state,
                &session,
                &locale,
                template,
                "Templates with dynamic documents are not supported yet",
            );
        }
        let prefs = &resubmitted.preferences.0;
        let values = prefs
            .get("default_values")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !values.is_empty() {
            return unsupported(
                &state,
                &session,
                &locale,
                template,
                "Resubmitting with default values is not supported yet",
            );
        }
        let preferences = if blank(prefs) {
            json!({ "send_email": true })
        } else {
            prefs.clone()
        };
        let metadata = if blank(&resubmitted.metadata.0) {
            json!({})
        } else {
            resubmitted.metadata.0.clone()
        };
        let source = if form.submission.source == "self" {
            "self"
        } else {
            "link"
        };
        create_submission(
            &state,
            template,
            NewParty {
                email: resubmitted.email.clone(),
                phone: resubmitted.phone.clone(),
                name: resubmitted.name.clone(),
                values,
                preferences,
                metadata,
            },
            source,
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?
    };
    Ok(start_redirect(&state, &headers, &submitter.slug))
}
