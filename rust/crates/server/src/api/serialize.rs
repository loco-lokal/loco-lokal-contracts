//! JSON shapes of the API, ported from `Templates::SerializeForApi`,
//! `Submissions::SerializeForApi`, `Submitters::SerializeForApi` and
//! `Submitters::SerializeForWebhook`. Keys come in the order Rails' `as_json` emits them.
//!
//! Records are loaded once per response ([`SubmissionData`], [`TemplateData`]) so lists
//! do not query per row, mirroring the controllers' `preload` calls.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDateTime, Utc};
use loco_db::{
    JsonText,
    submissions::{self, Submission, SubmissionEvent, Submitter},
    templates::{self, Folder, Template},
    users::{self, UserBrief},
};
use loco_storage::{Attachment, Blob};
use serde_json::{Map, Value, json};

use crate::{errors::AppError, files, state::AppState};

/// Rails' JSON time format: UTC with milliseconds (`2026-11-07T10:00:00.123Z`).
#[must_use]
pub fn time(t: NaiveDateTime) -> Value {
    Value::String(t.and_utc().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

#[must_use]
pub fn opt_time(t: Option<NaiveDateTime>) -> Value {
    t.map_or(Value::Null, time)
}

fn str_or_null(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| Value::String(s.to_owned()))
}

/// URL context: link expiry for the account (`Accounts.link_expires_at`).
#[derive(Clone)]
pub struct Urls<'a> {
    pub state: &'a AppState,
    pub expires_at: Option<DateTime<Utc>>,
}

impl<'a> Urls<'a> {
    pub async fn new(state: &'a AppState, account_id: i64) -> Result<Self, AppError> {
        Ok(Self {
            state,
            expires_at: files::link_expires_at(state, account_id).await?,
        })
    }

    /// The proxy URL of a loaded blob (its uuid was ensured when loading).
    #[must_use]
    pub fn blob(&self, blob: &Blob) -> String {
        files::url_for(
            self.state,
            blob.uuid.as_deref().unwrap_or_default(),
            &blob.filename,
            self.expires_at,
        )
    }

    /// `"#{default_url}/s/#{slug}"` (`submit_form_url`).
    #[must_use]
    pub fn submit_form(&self, slug: &str) -> String {
        format!("{}/s/{slug}", self.state.default_url())
    }

    /// `submissions_preview_url(slug)`.
    #[must_use]
    pub fn submission_preview(&self, slug: &str) -> String {
        format!("{}/e/{slug}", self.state.default_url())
    }
}

/// Attachments with blobs, loaded for a set of records and given blob uuids.
async fn load_attachments(
    state: &AppState,
    record_type: &str,
    record_ids: &[i64],
    name: &str,
) -> Result<HashMap<i64, Vec<(Attachment, Blob)>>, AppError> {
    let mut map: HashMap<i64, Vec<(Attachment, Blob)>> = HashMap::new();
    if record_ids.is_empty() {
        return Ok(map);
    }
    for (attachment, mut blob) in
        Attachment::with_blobs_for_records(&state.db, record_type, record_ids, name).await?
    {
        blob.ensure_uuid(&state.db).await?;
        map.entry(attachment.record_id)
            .or_default()
            .push((attachment, blob));
    }
    Ok(map)
}

/// `ActiveStorage::Filename#base`: the filename without its extension.
#[must_use]
pub fn filename_base(filename: &str) -> String {
    match filename.rfind('.') {
        Some(i) if i > 0 => filename[..i].to_owned(),
        _ => filename.to_owned(),
    }
}

fn user_json(user: &UserBrief) -> Value {
    json!({
        "id": user.id,
        "email": user.email,
        "first_name": user.first_name,
        "last_name": user.last_name,
    })
}

// --- Templates -------------------------------------------------------------------------

/// What `Templates::SerializeForApi` needs besides the template rows.
pub struct TemplateData {
    authors: HashMap<i64, UserBrief>,
    folders: HashMap<i64, Folder>,
    documents: HashMap<i64, Vec<(Attachment, Blob)>>,
    /// First-page previews (`0.png` / `0.jpg`), by document attachment id.
    previews: HashMap<i64, Blob>,
}

impl TemplateData {
    pub async fn load(state: &AppState, list: &[Template]) -> Result<Self, AppError> {
        let ids: Vec<i64> = list.iter().map(|t| t.id).collect();
        let author_ids: Vec<i64> = list.iter().map(|t| t.author_id).collect();
        let folder_ids: Vec<i64> = list.iter().map(|t| t.folder_id).collect();

        let authors = users::find_briefs(&state.db, &author_ids)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect();
        let folders = templates::find_folders(&state.db, &folder_ids)
            .await?
            .into_iter()
            .map(|f| (f.id, f))
            .collect();
        let documents = load_attachments(state, "Template", &ids, "documents").await?;

        let document_ids: Vec<i64> = documents
            .values()
            .flatten()
            .map(|(attachment, _)| attachment.id)
            .collect();
        let mut previews = HashMap::new();
        for (record_id, items) in load_attachments(
            state,
            "ActiveStorage::Attachment",
            &document_ids,
            "preview_images",
        )
        .await?
        {
            if let Some((_, blob)) = items
                .into_iter()
                .find(|(_, blob)| blob.filename == "0.png" || blob.filename == "0.jpg")
            {
                previews.insert(record_id, blob);
            }
        }

        Ok(Self {
            authors,
            folders,
            documents,
            previews,
        })
    }

    #[must_use]
    pub fn folder_name(&self, folder_id: i64) -> Value {
        self.folders
            .get(&folder_id)
            .map_or(Value::Null, |f| Value::String(f.full_name.clone()))
    }
}

/// `Templates::SerializeForApi.call`.
#[must_use]
pub fn template_json(urls: &Urls<'_>, data: &TemplateData, template: &Template) -> Value {
    let mut json = Map::new();
    json.insert("id".into(), template.id.into());
    json.insert("name".into(), template.name.clone().into());
    json.insert("slug".into(), template.slug.clone().into());
    json.insert("schema".into(), template.schema.0.clone());
    json.insert("submitters".into(), template.submitters.0.clone());
    json.insert("fields".into(), template.fields.0.clone());
    json.insert(
        "variables_schema".into(),
        template
            .variables_schema
            .as_ref()
            .map(|v| v.0.clone())
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!({})),
    );
    json.insert("preferences".into(), template.preferences.0.clone());
    json.insert("source".into(), template.source.clone().into());
    json.insert("created_at".into(), time(template.created_at));
    json.insert("updated_at".into(), time(template.updated_at));
    json.insert("archived_at".into(), opt_time(template.archived_at));
    json.insert("author_id".into(), template.author_id.into());
    json.insert(
        "external_id".into(),
        str_or_null(template.external_id.as_deref()),
    );
    json.insert("folder_id".into(), template.folder_id.into());
    json.insert("shared_link".into(), template.shared_link.into());
    json.insert(
        "application_key".into(),
        str_or_null(template.external_id.as_deref()),
    );
    json.insert("folder_name".into(), data.folder_name(template.folder_id));
    if let Some(author) = data.authors.get(&template.author_id) {
        json.insert("author".into(), user_json(author));
    }
    json.insert(
        "documents".into(),
        Value::Array(template_documents(urls, data, template)),
    );
    Value::Object(json)
}

/// `build_documents_array`: the schema's documents with their first-page preview.
/// Dynamic documents (Phase 5) are listed only once they have a stored attachment.
fn template_documents(urls: &Urls<'_>, data: &TemplateData, template: &Template) -> Vec<Value> {
    let documents = data.documents.get(&template.id);
    template
        .schema
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let uuid = item.get("attachment_uuid")?.as_str()?;
            let (attachment, blob) = documents?.iter().find(|(a, _)| a.uuid == uuid)?;
            let preview = data.previews.get(&attachment.id);
            Some(json!({
                "id": attachment.id,
                "uuid": attachment.uuid,
                "url": urls.blob(blob),
                "preview_image_url": preview.map(|b| urls.blob(b)),
                "filename": blob.filename,
            }))
        })
        .collect()
}

// --- Submissions and submitters --------------------------------------------------------

/// Which associations to load for [`SubmissionData`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Load {
    pub events: bool,
    pub documents: bool,
    pub attachments: bool,
}

/// Submissions with everything their serializers read.
pub struct SubmissionData {
    pub submitters: HashMap<i64, Vec<Submitter>>,
    pub templates: HashMap<i64, Template>,
    folders: HashMap<i64, Folder>,
    users: HashMap<i64, UserBrief>,
    events: HashMap<i64, Vec<SubmissionEvent>>,
    submitter_documents: HashMap<i64, Vec<(Attachment, Blob)>>,
    submitter_attachments: HashMap<i64, Vec<(Attachment, Blob)>>,
    audit_trails: HashMap<i64, Vec<(Attachment, Blob)>>,
    combined_documents: HashMap<i64, Vec<(Attachment, Blob)>>,
}

impl SubmissionData {
    pub async fn load(state: &AppState, list: &[Submission], load: Load) -> Result<Self, AppError> {
        let ids: Vec<i64> = list.iter().map(|s| s.id).collect();
        let mut submitters: HashMap<i64, Vec<Submitter>> = HashMap::new();
        for submitter in submissions::submitters_for(&state.db, &ids).await? {
            submitters
                .entry(submitter.submission_id)
                .or_default()
                .push(submitter);
        }
        let mut data = Self::with_submitters(state, list, submitters, load).await?;
        data.audit_trails = load_attachments(state, "Submission", &ids, "audit_trail").await?;
        data.combined_documents =
            load_attachments(state, "Submission", &ids, "combined_document").await?;
        Ok(data)
    }

    /// For submitters that were loaded separately (`/api/submitters`).
    pub async fn with_submitters(
        state: &AppState,
        list: &[Submission],
        submitters: HashMap<i64, Vec<Submitter>>,
        load: Load,
    ) -> Result<Self, AppError> {
        let ids: Vec<i64> = list.iter().map(|s| s.id).collect();
        let template_ids: Vec<i64> = list.iter().filter_map(|s| s.template_id).collect();
        let templates: HashMap<i64, Template> = templates::find_many(&state.db, &template_ids)
            .await?
            .into_iter()
            .map(|t| (t.id, t))
            .collect();
        let folder_ids: Vec<i64> = templates.values().map(|t| t.folder_id).collect();
        let folders = templates::find_folders(&state.db, &folder_ids)
            .await?
            .into_iter()
            .map(|f| (f.id, f))
            .collect();
        let user_ids: Vec<i64> = list.iter().filter_map(|s| s.created_by_user_id).collect();
        let users = users::find_briefs(&state.db, &user_ids)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect();

        let mut events: HashMap<i64, Vec<SubmissionEvent>> = HashMap::new();
        if load.events {
            for event in submissions::events_for_submissions(&state.db, &ids).await? {
                events.entry(event.submission_id).or_default().push(event);
            }
        }

        let submitter_ids: Vec<i64> = submitters.values().flatten().map(|s| s.id).collect();
        let submitter_documents = if load.documents {
            load_attachments(state, "Submitter", &submitter_ids, "documents").await?
        } else {
            HashMap::new()
        };
        let submitter_attachments = if load.attachments {
            load_attachments(state, "Submitter", &submitter_ids, "attachments").await?
        } else {
            HashMap::new()
        };

        Ok(Self {
            submitters,
            templates,
            folders,
            users,
            events,
            submitter_documents,
            submitter_attachments,
            audit_trails: HashMap::new(),
            combined_documents: HashMap::new(),
        })
    }

    #[must_use]
    pub fn template(&self, submission: &Submission) -> Option<&Template> {
        submission
            .template_id
            .and_then(|id| self.templates.get(&id))
    }

    #[must_use]
    pub fn submitters_of(&self, submission_id: i64) -> &[Submitter] {
        self.submitters
            .get(&submission_id)
            .map_or(&[], Vec::as_slice)
    }

    fn folder_name(&self, template: &Template) -> Value {
        self.folders
            .get(&template.folder_id)
            .map_or(Value::Null, |f| Value::String(f.full_name.clone()))
    }

    /// `submission.template_fields.presence || submission.template&.fields || []`.
    #[must_use]
    pub fn fields<'d>(&'d self, submission: &'d Submission) -> &'d [Value] {
        submission
            .template_fields
            .as_ref()
            .and_then(|f| f.as_array())
            .filter(|f| !f.is_empty())
            .or_else(|| self.template(submission).and_then(|t| t.fields.as_array()))
            .map_or(&[], Vec::as_slice)
    }

    /// `(submission.template_submitters || submission.template.submitters)`.
    #[must_use]
    pub fn template_submitters<'d>(&'d self, submission: &'d Submission) -> &'d [Value] {
        submission
            .template_submitters
            .as_ref()
            .and_then(|f| f.as_array())
            .or_else(|| {
                self.template(submission)
                    .and_then(|t| t.submitters.as_array())
            })
            .map_or(&[], Vec::as_slice)
    }

    fn submitter_events(&self, submitter: &Submitter) -> Vec<&SubmissionEvent> {
        self.events
            .get(&submitter.submission_id)
            .into_iter()
            .flatten()
            .filter(|e| e.submitter_id == Some(submitter.id))
            .collect()
    }

    fn first_attachment(map: &HashMap<i64, Vec<(Attachment, Blob)>>, id: i64) -> Option<&Blob> {
        map.get(&id).and_then(|v| v.first()).map(|(_, b)| b)
    }

    #[must_use]
    pub fn audit_log_url(&self, urls: &Urls<'_>, submission: &Submission) -> Value {
        Self::first_attachment(&self.audit_trails, submission.id)
            .map_or(Value::Null, |b| urls.blob(b).into())
    }

    #[must_use]
    pub fn combined_document_url(&self, urls: &Urls<'_>, submission: &Submission) -> Value {
        Self::first_attachment(&self.combined_documents, submission.id)
            .map_or(Value::Null, |b| urls.blob(b).into())
    }

    /// The template's role name for the submitter.
    #[must_use]
    pub fn role(&self, submission: &Submission, submitter: &Submitter) -> Value {
        self.template_submitters(submission)
            .iter()
            .find(|s| s.get("uuid").and_then(Value::as_str) == Some(submitter.uuid.as_str()))
            .and_then(|s| s.get("name").cloned())
            .unwrap_or(Value::Null)
    }
}

/// `Submitters::SerializeForApi.serialize_events`.
#[must_use]
pub fn events_json(events: &[&SubmissionEvent]) -> Value {
    Value::Array(
        events
            .iter()
            .map(|event| {
                let mut data = Map::new();
                for key in [
                    "reason",
                    "firstname",
                    "lastname",
                    "method",
                    "country",
                    "idcode",
                ] {
                    if let Some(value) = event.data.get(key) {
                        data.insert(key.into(), value.clone());
                    }
                }
                json!({
                    "id": event.id,
                    "submitter_id": event.submitter_id,
                    "event_type": event.event_type,
                    "event_timestamp": time(event.event_timestamp),
                    "data": data,
                })
            })
            .collect(),
    )
}

/// `"#{type.titleize} Field #{n}"` for unnamed fields.
fn default_field_name(field_type: &str, n: usize) -> String {
    let title = field_type
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{title} Field {n}")
}

/// The submitter's fields with their values (`build_values_array` /
/// `build_fields_array`).
fn field_values(
    urls: &Urls<'_>,
    data: &SubmissionData,
    submission: &Submission,
    submitter: &Submitter,
    as_fields: bool,
) -> Value {
    let attachments: HashMap<&str, &Blob> = data
        .submitter_attachments
        .get(&submitter.id)
        .into_iter()
        .flatten()
        .map(|(a, b)| (a.uuid.as_str(), b))
        .collect();
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut items = Vec::new();

    for field in data.fields(submission) {
        let field_type = field.get("type").and_then(Value::as_str).unwrap_or("");
        let counter = counters.entry(field_type.to_owned()).or_insert(0);
        *counter += 1;
        let n = *counter;

        if field.get("submitter_uuid").and_then(Value::as_str) != Some(submitter.uuid.as_str())
            || field_type == "heading"
        {
            continue;
        }
        let uuid = field.get("uuid").and_then(Value::as_str).unwrap_or("");
        let name = field
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .map_or_else(|| default_field_name(field_type, n), str::to_owned);

        let stored = submitter.values.get(uuid);
        if stored.is_none() && submitter.completed_at.is_none() {
            continue;
        }
        let stored = stored.cloned().unwrap_or(Value::Null);
        let url = |id: &Value| {
            id.as_str()
                .and_then(|id| attachments.get(id))
                .map(|b| Value::String(urls.blob(b)))
        };
        let value = match field_type {
            "image" | "signature" | "initials" | "stamp" | "payment" | "kba" => {
                url(&stored).unwrap_or(Value::Null)
            }
            "file" => {
                let ids = match &stored {
                    Value::Array(items) => items.clone(),
                    Value::Null => Vec::new(),
                    other => vec![other.clone()],
                };
                Value::Array(ids.iter().filter_map(url).collect())
            }
            _ => stored,
        };

        items.push(if as_fields {
            json!({
                "name": name,
                "uuid": uuid,
                "value": value,
                "readonly": field.get("readonly") == Some(&Value::Bool(true)),
            })
        } else {
            json!({ "field": name, "value": value })
        });
    }
    Value::Array(items)
}

fn submitter_documents(urls: &Urls<'_>, data: &SubmissionData, submitter: &Submitter) -> Value {
    Value::Array(
        data.submitter_documents
            .get(&submitter.id)
            .into_iter()
            .flatten()
            .map(|(_, blob)| json!({ "name": filename_base(&blob.filename), "url": urls.blob(blob) }))
            .collect(),
    )
}

/// Options of `Submitters::SerializeForApi.call`.
#[derive(Clone, Copy, Debug)]
pub struct SubmitterOpts {
    pub with_template: bool,
    pub with_events: bool,
    pub with_documents: bool,
    pub with_urls: bool,
    pub with_values: bool,
    /// `params[:include]` contains `fields`.
    pub include_fields: bool,
}

impl Default for SubmitterOpts {
    fn default() -> Self {
        Self {
            with_template: false,
            with_events: false,
            with_documents: true,
            with_urls: false,
            with_values: true,
            include_fields: false,
        }
    }
}

/// The `as_json(only: …, methods: %i[status application_key])` part shared by the
/// submitter serializers.
fn submitter_base(submitter: &Submitter) -> Map<String, Value> {
    let mut json = Map::new();
    json.insert("id".into(), submitter.id.into());
    json.insert("slug".into(), submitter.slug.clone().into());
    json.insert("uuid".into(), submitter.uuid.clone().into());
    json.insert("name".into(), str_or_null(submitter.name.as_deref()));
    json.insert("email".into(), str_or_null(submitter.email.as_deref()));
    json.insert("phone".into(), str_or_null(submitter.phone.as_deref()));
    json.insert("completed_at".into(), opt_time(submitter.completed_at));
    json.insert("declined_at".into(), opt_time(submitter.declined_at));
    json.insert(
        "external_id".into(),
        str_or_null(submitter.external_id.as_deref()),
    );
    json.insert("submission_id".into(), submitter.submission_id.into());
    json.insert("metadata".into(), submitter.metadata.0.clone());
    json.insert("opened_at".into(), opt_time(submitter.opened_at));
    json.insert("sent_at".into(), opt_time(submitter.sent_at));
    json.insert("created_at".into(), time(submitter.created_at));
    json.insert("updated_at".into(), time(submitter.updated_at));
    json.insert("status".into(), submitter.status().into());
    json.insert(
        "application_key".into(),
        str_or_null(submitter.external_id.as_deref()),
    );
    json
}

/// `submitter.preferences.except('default_values')`.
fn preferences(submitter: &Submitter) -> Value {
    let mut preferences = submitter.preferences.0.clone();
    if let Value::Object(map) = &mut preferences {
        map.remove("default_values");
    }
    preferences
}

/// `Submitters::SerializeForApi.call`.
#[must_use]
pub fn submitter_json(
    urls: &Urls<'_>,
    data: &SubmissionData,
    submission: &Submission,
    submitter: &Submitter,
    opts: SubmitterOpts,
) -> Value {
    let mut json = submitter_base(submitter);

    if opts.include_fields {
        json.insert(
            "fields".into(),
            field_values(urls, data, submission, submitter, true),
        );
    }
    if opts.with_template {
        json.insert(
            "template".into(),
            data.template(submission).map_or(Value::Null, |t| {
                json!({
                    "id": t.id,
                    "name": t.name,
                    "created_at": time(t.created_at),
                    "updated_at": time(t.updated_at),
                })
            }),
        );
    }
    if opts.with_values {
        json.insert(
            "values".into(),
            field_values(urls, data, submission, submitter, false),
        );
    }
    if opts.with_documents {
        json.insert(
            "documents".into(),
            submitter_documents(urls, data, submitter),
        );
    }
    json.insert("preferences".into(), preferences(submitter));
    if opts.with_events {
        json.insert(
            "submission_events".into(),
            events_json(&data.submitter_events(submitter)),
        );
    }
    json.insert("role".into(), data.role(submission, submitter));
    if opts.with_urls {
        json.insert("embed_src".into(), urls.submit_form(&submitter.slug).into());
    }
    Value::Object(json)
}

/// Options of `Submissions::SerializeForApi.call`.
#[derive(Clone, Copy, Debug)]
pub struct SubmissionOpts {
    pub with_events: bool,
    pub with_documents: bool,
    pub with_values: bool,
    pub include_fields: bool,
}

impl Default for SubmissionOpts {
    fn default() -> Self {
        Self {
            with_events: true,
            with_documents: true,
            with_values: true,
            include_fields: false,
        }
    }
}

/// `Submissions::SerializeForApi.build_status`.
#[must_use]
pub fn submission_status(submission: &Submission, submitters: &[Submitter]) -> &'static str {
    if submission.completed_at.is_some() {
        "completed"
    } else if submitters.iter().any(|s| s.declined_at.is_some()) {
        "declined"
    } else if submission.expired(Utc::now().naive_utc()) {
        "expired"
    } else {
        "pending"
    }
}

fn last_completed(submitters: &[Submitter]) -> Option<&Submitter> {
    submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| s.completed_at)
}

/// `Submissions::SerializeForApi.call`.
#[must_use]
pub fn submission_json(
    urls: &Urls<'_>,
    data: &SubmissionData,
    submission: &Submission,
    opts: SubmissionOpts,
) -> Value {
    let submitters = data.submitters_of(submission.id);
    let submitter_opts = SubmitterOpts {
        with_documents: opts.with_documents,
        with_values: opts.with_values,
        include_fields: opts.include_fields,
        ..SubmitterOpts::default()
    };
    let serialized: Vec<Value> = submitters
        .iter()
        .map(|s| submitter_json(urls, data, submission, s, submitter_opts))
        .collect();

    let mut json = Map::new();
    json.insert("id".into(), submission.id.into());
    json.insert("name".into(), str_or_null(submission.name.as_deref()));
    json.insert("slug".into(), submission.slug.clone().into());
    json.insert("source".into(), submission.source.clone().into());
    json.insert(
        "submitters_order".into(),
        submission.submitters_order.clone().into(),
    );
    json.insert("expire_at".into(), opt_time(submission.expire_at));
    json.insert("created_at".into(), time(submission.created_at));
    json.insert("updated_at".into(), time(submission.updated_at));
    json.insert("archived_at".into(), opt_time(submission.archived_at));
    // Replaced by the full submitters below, keeping this position.
    json.insert("submitters".into(), Value::Null);
    if let Some(template) = data.template(submission) {
        json.insert(
            "template".into(),
            json!({
                "id": template.id,
                "name": template.name,
                "external_id": template.external_id,
                "created_at": time(template.created_at),
                "updated_at": time(template.updated_at),
                "folder_name": data.folder_name(template),
            }),
        );
    }
    let created_by_user = submission
        .created_by_user_id
        .and_then(|id| data.users.get(&id))
        .map(user_json);
    if let Some(user) = &created_by_user {
        json.insert("created_by_user".into(), user.clone());
    }
    json.insert(
        "variables".into(),
        submission
            .variables
            .as_ref()
            .map(|v| v.0.clone())
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!({})),
    );
    if created_by_user.is_none() {
        json.insert("created_by_user".into(), Value::Null);
    }
    if opts.with_events {
        let events: Vec<&SubmissionEvent> = data
            .events
            .get(&submission.id)
            .into_iter()
            .flatten()
            .collect();
        json.insert("submission_events".into(), events_json(&events));
    }
    if opts.include_fields {
        json.insert(
            "fields".into(),
            submission
                .template_fields
                .as_ref()
                .map(|f| f.0.clone())
                .or_else(|| data.template(submission).map(|t| t.fields.0.clone()))
                .unwrap_or(Value::Null),
        );
    }

    if submission.completed_at.is_some() {
        if opts.with_documents {
            let documents = last_completed(submitters)
                .and_then(|last| {
                    submitters
                        .iter()
                        .position(|s| s.id == last.id)
                        .and_then(|i| serialized[i].get("documents").cloned())
                })
                .unwrap_or_else(|| json!([]));
            json.insert("documents".into(), documents);
        }
        json.insert("audit_log_url".into(), data.audit_log_url(urls, submission));
        json.insert(
            "combined_document_url".into(),
            data.combined_document_url(urls, submission),
        );
        json.insert("status".into(), "completed".into());
        json.insert("completed_at".into(), opt_time(submission.completed_at));
    } else {
        if opts.with_documents {
            json.insert("documents".into(), json!([]));
        }
        json.insert("audit_log_url".into(), Value::Null);
        json.insert("combined_document_url".into(), Value::Null);
        json.insert(
            "status".into(),
            submission_status(submission, submitters).into(),
        );
        json.insert("completed_at".into(), Value::Null);
    }

    json.insert("submitters".into(), Value::Array(serialized));
    Value::Object(json)
}

/// `Submitters::SerializeForWebhook.call`: the `form.*` event payload.
#[must_use]
pub fn webhook_submitter_json(
    urls: &Urls<'_>,
    data: &SubmissionData,
    submission: &Submission,
    submitter: &Submitter,
) -> Value {
    let mut json = Map::new();
    json.insert("id".into(), submitter.id.into());
    json.insert("submission_id".into(), submitter.submission_id.into());
    json.insert("email".into(), str_or_null(submitter.email.as_deref()));
    json.insert("phone".into(), str_or_null(submitter.phone.as_deref()));
    json.insert("name".into(), str_or_null(submitter.name.as_deref()));
    json.insert("ua".into(), str_or_null(submitter.ua.as_deref()));
    json.insert("ip".into(), str_or_null(submitter.ip.as_deref()));
    json.insert("sent_at".into(), opt_time(submitter.sent_at));
    json.insert("opened_at".into(), opt_time(submitter.opened_at));
    json.insert("completed_at".into(), opt_time(submitter.completed_at));
    json.insert("declined_at".into(), opt_time(submitter.declined_at));
    json.insert("created_at".into(), time(submitter.created_at));
    json.insert("updated_at".into(), time(submitter.updated_at));
    json.insert(
        "external_id".into(),
        str_or_null(submitter.external_id.as_deref()),
    );
    json.insert("metadata".into(), submitter.metadata.0.clone());
    json.insert("status".into(), submitter.status().into());
    json.insert(
        "application_key".into(),
        str_or_null(submitter.external_id.as_deref()),
    );

    let decline_reason = if submitter.declined_at.is_some() {
        data.submitter_events(submitter)
            .into_iter()
            .find(|e| e.event_type == "decline_form")
            .and_then(|e| e.data.get("reason").cloned())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    json.insert("decline_reason".into(), decline_reason);
    json.insert("role".into(), data.role(submission, submitter));
    json.insert("preferences".into(), preferences(submitter));
    json.insert(
        "values".into(),
        field_values(urls, data, submission, submitter, false),
    );
    json.insert(
        "documents".into(),
        submitter_documents(urls, data, submitter),
    );
    json.insert("audit_log_url".into(), data.audit_log_url(urls, submission));
    json.insert(
        "submission_url".into(),
        urls.submission_preview(&submission.slug).into(),
    );
    json.insert(
        "template".into(),
        data.template(submission).map_or(Value::Null, |t| {
            json!({
                "id": t.id,
                "name": t.name,
                "external_id": t.external_id,
                "created_at": time(t.created_at),
                "updated_at": time(t.updated_at),
                "folder_name": data.folder_name(t),
            })
        }),
    );
    let submitters = data.submitters_of(submission.id);
    json.insert(
        "submission".into(),
        json!({
            "id": submission.id,
            "audit_log_url": data.audit_log_url(urls, submission),
            "combined_document_url": data.combined_document_url(urls, submission),
            "status": submission_status(submission, submitters),
            "url": urls.submission_preview(&submission.slug),
            "variables": submission.variables.as_ref().map(|v| v.0.clone())
                .filter(|v| !v.is_null()).unwrap_or_else(|| json!({})),
            "created_at": time(submission.created_at),
        }),
    );
    Value::Object(json)
}

/// `JsonText` contents, or an empty object.
#[must_use]
pub fn object_or_empty(value: Option<&JsonText>) -> Value {
    value
        .map(|v| v.0.clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times_like_rails() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 7)
            .unwrap()
            .and_hms_micro_opt(0, 54, 24, 942_123)
            .unwrap();
        assert_eq!(time(t), json!("2026-11-07T00:54:24.942Z"));
    }

    #[test]
    fn names_unnamed_fields() {
        assert_eq!(default_field_name("text", 2), "Text Field 2");
        assert_eq!(
            default_field_name("phone_number", 1),
            "Phone Number Field 1"
        );
        assert_eq!(filename_base("contract.v2.pdf"), "contract.v2");
        assert_eq!(filename_base(".env"), ".env");
    }
}
