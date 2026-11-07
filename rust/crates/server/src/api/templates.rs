//! `Api::TemplatesController` and `Api::TemplatesCloneController`.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::Utc;
use loco_db::{
    JsonText, search,
    templates::{self, NewTemplate, Template, TemplateFilter, TemplateUpdate},
    users,
};
use loco_domain::ability::{Action, SHARING_ALL_ID};
use loco_storage::Attachment;
use serde_json::{Value, json};

use super::{
    Params, access_denied, like_pattern, ok, page, pagination,
    permit::{Rule, permit},
    serialize::{self, TemplateData, Urls},
};
use crate::{
    auth::{ApiAuth, Auth},
    errors::AppError,
    purge,
    state::AppState,
    webhooks::{self, EventRecord},
};

const SUBMITTER_RULES: &[(&str, Rule)] = &[
    ("name", Rule::Scalar),
    ("uuid", Rule::Scalar),
    ("is_requester", Rule::Scalar),
    ("invite_by_uuid", Rule::Scalar),
    ("invite_via_field_uuid", Rule::Scalar),
    ("optional_invite_by_uuid", Rule::Scalar),
    ("linked_to_uuid", Rule::Scalar),
    ("email", Rule::Scalar),
    ("order", Rule::Scalar),
];

const AREA_RULES: &[(&str, Rule)] = &[
    ("uuid", Rule::Scalar),
    ("x", Rule::Scalar),
    ("y", Rule::Scalar),
    ("w", Rule::Scalar),
    ("h", Rule::Scalar),
    ("cell_w", Rule::Scalar),
    ("attachment_uuid", Rule::Scalar),
    ("option_uuid", Rule::Scalar),
    ("page", Rule::Scalar),
];

const FIELD_RULES: &[(&str, Rule)] = &[
    ("uuid", Rule::Scalar),
    ("submitter_uuid", Rule::Scalar),
    ("name", Rule::Scalar),
    ("type", Rule::Scalar),
    ("required", Rule::Scalar),
    ("readonly", Rule::Scalar),
    ("default_value", Rule::ScalarOrScalars),
    ("title", Rule::Scalar),
    ("description", Rule::Scalar),
    ("prefillable", Rule::Scalar),
    ("preferences", Rule::AnyHash),
    (
        "conditions",
        Rule::Objects(&[
            ("field_uuid", Rule::Scalar),
            ("value", Rule::Scalar),
            ("action", Rule::Scalar),
            ("operation", Rule::Scalar),
        ]),
    ),
    (
        "options",
        Rule::Objects(&[("value", Rule::Scalar), ("uuid", Rule::Scalar)]),
    ),
    (
        "validation",
        Rule::Object(&[
            ("message", Rule::Scalar),
            ("pattern", Rule::Scalar),
            ("min", Rule::Scalar),
            ("max", Rule::Scalar),
            ("step", Rule::Scalar),
        ]),
    ),
    ("areas", Rule::Objects(AREA_RULES)),
];

const TEMPLATE_RULES: &[(&str, Rule)] = &[
    ("name", Rule::Scalar),
    ("external_id", Rule::Scalar),
    ("shared_link", Rule::Scalar),
    ("submitters", Rule::Objects(SUBMITTER_RULES)),
    ("fields", Rule::Objects(FIELD_RULES)),
];

/// The template, or the response for a missing or forbidden one
/// (`load_and_authorize_resource :template`).
pub(super) async fn authorized_template(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    id: &str,
    action: Action,
) -> Result<Result<Template, Response>, AppError> {
    let Ok(id) = id.parse::<i64>() else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let Some(template) = templates::find(&state.db, id).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if can(state, auth, &template, action).await? {
        Ok(Ok(template))
    } else {
        Ok(Err(access_denied(
            state,
            auth,
            headers,
            "Template",
            template.id,
            Some(template.account_id),
        )
        .await?))
    }
}

pub(super) async fn can(
    state: &AppState,
    auth: &Auth,
    template: &Template,
    action: Action,
) -> Result<bool, AppError> {
    let sharings = if template.account_id == auth.account.id {
        Vec::new()
    } else {
        templates::sharings(&state.db, template.id).await?
    };
    Ok(auth
        .ability()
        .can_template(action, Some(template.account_id), &sharings))
}

/// `GET /api/templates`.
pub async fn index(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    params: Params,
) -> Result<Response, AppError> {
    let shared = params.is_true("shared");
    let shared_with = shared_account_ids(&auth);
    if shared && shared_with.is_none() {
        // `Templates.shared` is `Template.none` for accounts without a linked account.
        return Ok(ok(&json!({ "data": [], "pagination": pagination(&[]) })));
    }

    let (name_like, search_ids) = match params.str("q") {
        None => (None, None),
        Some(q) if search::enabled(&state.db).await? => {
            // `search_shared` looks in the linked parent account's index.
            let accounts: Vec<i64> = if shared {
                users::linked_parent_account_id(&state.db, auth.account.id)
                    .await?
                    .into_iter()
                    .collect()
            } else {
                vec![auth.account.id]
            };
            let ids = search::matching_ids(&state.db, "Template", &accounts, &q, false).await?;
            (None, Some(ids))
        }
        Some(q) => (Some(like_pattern(&q)), None),
    };
    let folder_ids = match params.str("folder") {
        Some(folder) => {
            Some(templates::filter_folders_by_full_name(&state.db, auth.account.id, &folder).await?)
        }
        None => None,
    };
    let application_key = params.str("application_key");
    let external_id = params.str("external_id");
    // Both filters apply when both are given.
    if let (Some(a), Some(b)) = (&application_key, &external_id)
        && a != b
    {
        return Ok(ok(&json!({ "data": [], "pagination": pagination(&[]) })));
    }
    let slug = params.str("slug");

    let filter = TemplateFilter {
        account_id: (!shared).then_some(auth.account.id),
        shared_with: if shared { shared_with.as_deref() } else { None },
        name_like: name_like.as_deref(),
        ids: search_ids.as_deref(),
        archived: params.is_true("archived"),
        external_id: application_key.as_deref().or(external_id.as_deref()),
        slug: slug.as_deref(),
        folder_ids: folder_ids.as_deref(),
    };
    let list = templates::list(&state.db, &filter, page(&params)).await?;

    let urls = Urls::new(&state, auth.account.id).await?;
    let data = TemplateData::load(&state, &list).await?;
    let ids: Vec<i64> = list.iter().map(|t| t.id).collect();
    Ok(ok(&json!({
        "data": list.iter().map(|t| serialize::template_json(&urls, &data, t)).collect::<Vec<_>>(),
        "pagination": pagination(&ids),
    })))
}

/// `Templates.shared`: the accounts whose sharings count (`[account_id, ALL_ID]`, or the
/// account alone for testing accounts); `None` without a linked account.
fn shared_account_ids(auth: &Auth) -> Option<Vec<i64>> {
    if !auth.account.linked {
        return None;
    }
    let mut ids = vec![auth.account.id];
    if !auth.account.testing {
        ids.push(SHARING_ALL_ID);
    }
    Some(ids)
}

/// `GET /api/templates/:id`.
pub async fn show(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let template = match authorized_template(&state, &auth, &headers, &id, Action::Read).await? {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };
    let urls = Urls::new(&state, template.account_id).await?;
    let data = TemplateData::load(&state, std::slice::from_ref(&template)).await?;
    Ok(ok(&serialize::template_json(&urls, &data, &template)))
}

/// `value.blank?`.
#[must_use]
pub fn blank(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Active Model's boolean cast: `false`, `0`, `"0"`, `"f"`, `"false"`, `"off"` (any case)
/// are false, blank is nil, anything else true.
#[must_use]
pub fn cast_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_f64() != Some(0.0)),
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(!matches!(
            s.to_ascii_lowercase().as_str(),
            "0" | "f" | "false" | "off"
        )),
        _ => Some(true),
    }
}

/// Active Model's string cast.
#[must_use]
pub fn cast_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_owned()),
        other => Some(other.to_string()),
    }
}

/// `PUT /api/templates/:id`.
pub async fn update(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let template = match authorized_template(&state, &auth, &headers, &id, Action::Update).await? {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };
    let nested = params.get("template").filter(|v| v.is_object());
    let nested_get = |key: &str| nested.and_then(|n| n.get(key));

    let mut changes = TemplateUpdate {
        name: template.name.clone(),
        external_id: template.external_id.clone(),
        shared_link: template.shared_link,
        submitters: template.submitters.clone(),
        fields: template.fields.clone(),
        folder_id: template.folder_id,
        archived_at: template.archived_at,
    };

    if let Some(folder_name) = params
        .get("folder_name")
        .or_else(|| nested_get("folder_name"))
        .and_then(Value::as_str)
    {
        changes.folder_id = templates::find_or_create_folder_by_name(
            &state.db,
            auth.account.id,
            auth.user.id,
            folder_name,
        )
        .await?;
    }

    let roles = params
        .get("roles")
        .filter(|v| !blank(v))
        .or_else(|| nested_get("roles").filter(|v| !blank(v)));
    if let Some(roles) = roles {
        let roles: Vec<Value> = match roles {
            Value::Array(items) => items.clone(),
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        };
        let mut submitters = changes.submitters.as_array().cloned().unwrap_or_default();
        for (index, role) in roles.into_iter().enumerate() {
            if let Some(item) = submitters.get_mut(index).and_then(Value::as_object_mut) {
                item.insert("name".into(), role);
            } else {
                submitters.push(json!({ "name": role, "uuid": uuid::Uuid::new_v4().to_string() }));
            }
        }
        changes.submitters = JsonText(Value::Array(submitters));
    }

    let archived = if params.has("archived") {
        params.get("archived")
    } else {
        nested_get("archived")
    };
    if let Some(Value::Bool(archived)) = archived
        && can(&state, &auth, &template, Action::Destroy).await?
    {
        changes.archived_at = archived.then(|| Utc::now().naive_utc());
    }

    let attrs = permit(nested.unwrap_or(&params.0), TEMPLATE_RULES);
    if let Some(name) = attrs.get("name") {
        match super::create::strip(cast_string(name)) {
            Some(name) => changes.name = name,
            None => return Ok(super::unprocessable("Name can't be blank")),
        }
    }
    if let Some(external_id) = attrs.get("external_id") {
        changes.external_id = super::create::strip(cast_string(external_id));
    }
    if let Some(shared_link) = attrs.get("shared_link").and_then(cast_bool) {
        changes.shared_link = shared_link;
    }
    if let Some(submitters) = attrs.get("submitters") {
        changes.submitters = JsonText(submitters.clone());
    }
    if let Some(fields) = attrs.get("fields") {
        changes.fields = JsonText(fields.clone());
    }

    let updated_at = templates::update(&state.db, template.id, &changes).await?;
    let record = [EventRecord {
        id: template.id,
        account_id: template.account_id,
    }];
    if changes.name != template.name {
        crate::search::reindex_later(&state, "Template", &[template.id]).await;
    }
    webhooks::enqueue(&state, "template.updated", &record).await;
    if changes.archived_at.is_some() && changes.archived_at != template.archived_at {
        webhooks::enqueue(&state, "template.archived", &record).await;
    }

    Ok(ok(&json!({
        "id": template.id,
        "updated_at": serialize::time(updated_at),
    })))
}

/// `DELETE /api/templates/:id`: archives, or deletes with `permanently=true`.
pub async fn destroy(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let template = match authorized_template(&state, &auth, &headers, &id, Action::Destroy).await? {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };

    let archived_at = if params.is_true("permanently") {
        let mut tx = state.db.begin().await?;
        let blobs = loco_db::destroy::template(&mut tx, template.id).await?;
        tx.commit().await?;
        purge::purge_later(&state, blobs).await?;
        template.archived_at
    } else {
        let archived_at = templates::archive(&state.db, template.id).await?;
        webhooks::enqueue(
            &state,
            "template.archived",
            &[EventRecord {
                id: template.id,
                account_id: template.account_id,
            }],
        )
        .await;
        Some(archived_at)
    };

    Ok(ok(&json!({
        "id": template.id,
        "archived_at": serialize::opt_time(archived_at),
    })))
}

/// `Templates::Clone.update_submitters_and_fields_and_schema`: new uuids for roles and
/// fields, with every reference to them rewritten.
fn renew_uuids(
    submitters: &mut Value,
    fields: &mut Value,
    schema: &mut Value,
    preferences: &mut Value,
) {
    let mut submitter_map: HashMap<String, String> = HashMap::new();
    let mut field_map: HashMap<String, String> = HashMap::new();
    let lookup = |map: &HashMap<String, String>, value: Option<&Value>| -> Value {
        value
            .and_then(Value::as_str)
            .and_then(|old| map.get(old))
            .map_or(Value::Null, |new| Value::String(new.clone()))
    };

    for submitter in submitters.as_array_mut().into_iter().flatten() {
        let new = uuid::Uuid::new_v4().to_string();
        if let Some(old) = submitter.get("uuid").and_then(Value::as_str) {
            submitter_map.insert(old.to_owned(), new.clone());
        }
        submitter["uuid"] = Value::String(new);
    }
    if let Some(items) = preferences
        .get_mut("submitters")
        .and_then(Value::as_array_mut)
    {
        for submitter in items {
            let new = lookup(&submitter_map, submitter.get("uuid"));
            submitter["uuid"] = new;
        }
    }
    for field in fields.as_array_mut().into_iter().flatten() {
        let new = uuid::Uuid::new_v4().to_string();
        if let Some(old) = field.get("uuid").and_then(Value::as_str) {
            field_map.insert(old.to_owned(), new.clone());
        }
        field["uuid"] = Value::String(new);
        let submitter_uuid = lookup(&submitter_map, field.get("submitter_uuid"));
        field["submitter_uuid"] = submitter_uuid;
    }
    let rewrite_conditions = |item: &mut Value, map: &HashMap<String, String>| {
        let conditions = match item.get_mut("conditions") {
            Some(Value::Array(items)) => items.iter_mut().collect::<Vec<_>>(),
            Some(value @ Value::Object(_)) => vec![value],
            _ => Vec::new(),
        };
        for condition in conditions {
            let new = lookup(map, condition.get("field_uuid"));
            condition["field_uuid"] = new;
        }
    };
    for field in fields.as_array_mut().into_iter().flatten() {
        rewrite_conditions(field, &field_map);
        if let Some(formula) = field
            .pointer("/preferences/formula")
            .and_then(Value::as_str)
            .filter(|f| !f.trim().is_empty())
        {
            let mut formula = formula.to_owned();
            for (old, new) in &field_map {
                formula = formula.replace(old, new);
            }
            field["preferences"]["formula"] = Value::String(formula);
        }
    }
    for item in schema.as_array_mut().into_iter().flatten() {
        rewrite_conditions(item, &field_map);
    }
    for submitter in submitters.as_array_mut().into_iter().flatten() {
        for (key, map) in [
            ("optional_invite_by_uuid", &submitter_map),
            ("invite_by_uuid", &submitter_map),
            ("linked_to_uuid", &submitter_map),
            ("invite_via_field_uuid", &field_map),
        ] {
            if submitter.get(key).and_then(super::params::string).is_some() {
                let new = lookup(map, submitter.get(key));
                submitter[key] = new;
            }
        }
    }
}

/// `Templates::CloneAttachments`: new attachment uuids in the schema and field areas,
/// with optional new document names. Returns `(old uuid, new uuid)` pairs.
fn renew_attachment_uuids(
    schema: &mut Value,
    fields: &mut Value,
    documents: Option<&Value>,
) -> Vec<(String, String)> {
    let mut replacements = Vec::new();
    for (index, item) in schema.as_array_mut().into_iter().flatten().enumerate() {
        let new = uuid::Uuid::new_v4().to_string();
        if let Some(old) = item.get("attachment_uuid").and_then(Value::as_str) {
            replacements.push((old.to_owned(), new.clone()));
        }
        item["attachment_uuid"] = Value::String(new);
        if let Some(name) = documents
            .and_then(|d| d.get(index))
            .and_then(|d| d.get("name"))
            .and_then(super::params::string)
        {
            item["name"] = Value::String(name);
        }
    }
    for field in fields.as_array_mut().into_iter().flatten() {
        for area in field
            .get_mut("areas")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let new = area
                .get("attachment_uuid")
                .and_then(Value::as_str)
                .and_then(|old| replacements.iter().find(|(o, _)| o == old))
                .map(|(_, new)| new.clone());
            if let Some(new) = new {
                area["attachment_uuid"] = Value::String(new);
            }
        }
    }
    replacements
}

/// `POST /api/templates/:id/clone`.
pub async fn clone(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let original = match authorized_template(&state, &auth, &headers, &id, Action::Read).await? {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };
    if !can(&state, &auth, &original, Action::Create).await? {
        return access_denied(
            &state,
            &auth,
            &headers,
            "Template",
            original.id,
            Some(original.account_id),
        )
        .await;
    }

    let clone_suffix = state.views.t("en", "clone", &[]);
    let default_name = format!("{} ({clone_suffix})", original.name);
    let name = params.str("name");
    let new_name = name.clone().unwrap_or_else(|| default_name.clone());

    let folder_id = if let Some(folder_name) = params.str("folder_name") {
        templates::find_or_create_folder_by_name(
            &state.db,
            auth.account.id,
            auth.user.id,
            &folder_name,
        )
        .await?
    } else if auth.account.id == original.account_id {
        original.folder_id
    } else {
        templates::default_folder(&state.db, auth.account.id).await?
    };

    let mut submitters = original.submitters.0.clone();
    let mut fields = original.fields.0.clone();
    let mut schema = original.schema.0.clone();
    let mut preferences = original.preferences.0.clone();
    renew_uuids(&mut submitters, &mut fields, &mut schema, &mut preferences);

    if name.is_some()
        && schema.as_array().is_some_and(|s| s.len() == 1)
        && original.schema.pointer("/0/name").and_then(Value::as_str)
            == Some(original.name.as_str())
        && new_name != default_name
    {
        schema[0]["name"] = Value::String(new_name.clone());
    }
    let replacements = renew_attachment_uuids(&mut schema, &mut fields, params.get("documents"));

    let originals =
        Attachment::with_blobs_for_records(&state.db, "Template", &[original.id], "documents")
            .await?;
    let previews = Attachment::with_blobs_for_records(
        &state.db,
        "ActiveStorage::Attachment",
        &originals.iter().map(|(a, _)| a.id).collect::<Vec<_>>(),
        "preview_images",
    )
    .await?;

    let mut tx = state.db.begin().await?;
    let template = templates::insert(
        &mut *tx,
        &NewTemplate {
            name: new_name,
            slug: loco_rails_compat::base58(14),
            schema: JsonText(schema),
            fields: JsonText(fields),
            submitters: JsonText(submitters),
            author_id: auth.user.id,
            account_id: auth.account.id,
            source: "api".into(),
            folder_id,
            external_id: params
                .str("external_id")
                .or_else(|| params.str("application_key")),
            preferences: JsonText(preferences),
            shared_link: original.shared_link,
            variables_schema: original.variables_schema.clone(),
        },
    )
    .await?;
    templates::copy_accesses(&mut *tx, original.id, template.id).await?;
    // `original_template.schema_documents`: only documents in the schema are copied.
    for (attachment, _) in &originals {
        let Some((_, new_uuid)) = replacements.iter().find(|(old, _)| *old == attachment.uuid)
        else {
            continue;
        };
        let copy = Attachment::create_for_blob(
            &mut *tx,
            "Template",
            template.id,
            "documents",
            attachment.blob_id,
            new_uuid,
        )
        .await?;
        for (preview, _) in previews
            .iter()
            .filter(|(p, _)| p.record_id == attachment.id)
        {
            Attachment::create_for_blob(
                &mut *tx,
                "ActiveStorage::Attachment",
                copy.id,
                "preview_images",
                preview.blob_id,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        }
    }
    tx.commit().await?;

    webhooks::enqueue(
        &state,
        "template.created",
        &[EventRecord {
            id: template.id,
            account_id: template.account_id,
        }],
    )
    .await;
    crate::search::reindex_later(&state, "Template", &[template.id]).await;

    let urls = Urls::new(&state, template.account_id).await?;
    let data = TemplateData::load(&state, std::slice::from_ref(&template)).await?;
    Ok(ok(&serialize::template_json(&urls, &data, &template)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renews_clone_uuids_and_references() {
        let mut submitters = json!([
            {"uuid": "s1", "name": "A"},
            {"uuid": "s2", "name": "B", "invite_by_uuid": "s1", "invite_via_field_uuid": "f1"}
        ]);
        let mut fields = json!([
            {"uuid": "f1", "submitter_uuid": "s1", "areas": [{"attachment_uuid": "d1"}]},
            {"uuid": "f2", "submitter_uuid": "s2",
             "conditions": [{"field_uuid": "f1"}], "preferences": {"formula": "{{f1}} * 2"}}
        ]);
        let mut schema = json!([{"attachment_uuid": "d1", "name": "doc"}]);
        let mut preferences = json!({"submitters": [{"uuid": "s2"}]});
        renew_uuids(&mut submitters, &mut fields, &mut schema, &mut preferences);
        let replacements = renew_attachment_uuids(
            &mut schema,
            &mut fields,
            Some(&json!([{"name": "Renamed"}])),
        );

        let s1 = submitters[0]["uuid"].as_str().unwrap();
        let s2 = submitters[1]["uuid"].as_str().unwrap();
        let f1 = fields[0]["uuid"].as_str().unwrap();
        assert_ne!(s1, "s1");
        assert_eq!(submitters[1]["invite_by_uuid"], s1);
        assert_eq!(submitters[1]["invite_via_field_uuid"], f1);
        assert_eq!(fields[0]["submitter_uuid"], s1);
        assert_eq!(fields[1]["conditions"][0]["field_uuid"], f1);
        assert_eq!(
            fields[1]["preferences"]["formula"],
            format!("{{{{{f1}}}}} * 2")
        );
        assert_eq!(preferences["submitters"][0]["uuid"], s2);
        assert_eq!(replacements[0].0, "d1");
        assert_eq!(schema[0]["attachment_uuid"], replacements[0].1.as_str());
        assert_eq!(
            fields[0]["areas"][0]["attachment_uuid"],
            replacements[0].1.as_str()
        );
        assert_eq!(schema[0]["name"], "Renamed");
    }

    #[test]
    fn casts_like_active_model() {
        assert_eq!(cast_bool(&json!("false")), Some(false));
        assert_eq!(cast_bool(&json!("0")), Some(false));
        assert_eq!(cast_bool(&json!("yes")), Some(true));
        assert_eq!(cast_bool(&json!("")), None);
        assert_eq!(cast_string(&json!(12)), Some("12".into()));
    }
}
