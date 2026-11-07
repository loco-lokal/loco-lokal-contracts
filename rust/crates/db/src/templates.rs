//! `templates`, `template_folders` and `template_sharings`.

use chrono::NaiveDateTime;
use sqlx::{PgExecutor, PgPool};

use crate::{DbError, JsonText};

/// `TemplateFolder::DEFAULT_NAME`.
pub const DEFAULT_FOLDER_NAME: &str = "Default";

/// A `templates` row. JSON columns are in Rails' `serialize … coder: JSON` form.
#[derive(Clone, Debug)]
pub struct Template {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub schema: JsonText,
    pub fields: JsonText,
    pub submitters: JsonText,
    pub author_id: i64,
    pub account_id: i64,
    pub archived_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub source: String,
    pub folder_id: i64,
    pub external_id: Option<String>,
    pub preferences: JsonText,
    pub shared_link: bool,
    pub variables_schema: Option<JsonText>,
}

/// Filters of `GET /api/templates` (`Api::TemplatesController#filter_templates`).
#[derive(Clone, Debug, Default)]
pub struct TemplateFilter<'a> {
    pub account_id: Option<i64>,
    /// `Templates.shared`: ids of templates shared with these accounts and without
    /// template accesses.
    pub shared_with: Option<&'a [i64]>,
    /// `Templates.plain_search`: a lowercase `LIKE` pattern, already escaped.
    pub name_like: Option<&'a str>,
    /// `Templates.fulltext_search`: only these ids.
    pub ids: Option<&'a [i64]>,
    pub archived: bool,
    pub external_id: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub folder_ids: Option<&'a [i64]>,
}

/// Keyset pagination of the API (`ApiBaseController#paginate`): newest first.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub limit: i64,
    /// Only ids below this one.
    pub after: Option<i64>,
    /// Only ids above this one.
    pub before: Option<i64>,
}

pub async fn find(db: impl PgExecutor<'_>, id: i64) -> Result<Option<Template>, DbError> {
    Ok(sqlx::query_as!(
        Template,
        r#"SELECT t.id, t.slug, t.name, t.schema AS "schema: JsonText",
                      t.fields AS "fields: JsonText", t.submitters AS "submitters: JsonText",
                      t.author_id, t.account_id, t.archived_at, t.created_at, t.updated_at,
                      t.source, t.folder_id, t.external_id,
                      t.preferences AS "preferences: JsonText", t.shared_link,
                      t.variables_schema AS "variables_schema: JsonText"
               FROM templates t WHERE t.id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?)
}

pub async fn find_many(db: impl PgExecutor<'_>, ids: &[i64]) -> Result<Vec<Template>, DbError> {
    Ok(sqlx::query_as!(
        Template,
        r#"SELECT t.id, t.slug, t.name, t.schema AS "schema: JsonText",
                      t.fields AS "fields: JsonText", t.submitters AS "submitters: JsonText",
                      t.author_id, t.account_id, t.archived_at, t.created_at, t.updated_at,
                      t.source, t.folder_id, t.external_id,
                      t.preferences AS "preferences: JsonText", t.shared_link,
                      t.variables_schema AS "variables_schema: JsonText"
               FROM templates t WHERE t.id = ANY($1)"#,
        ids
    )
    .fetch_all(db)
    .await?)
}

pub async fn find_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> Result<Option<Template>, DbError> {
    Ok(sqlx::query_as!(
        Template,
        r#"SELECT t.id, t.slug, t.name, t.schema AS "schema: JsonText",
                      t.fields AS "fields: JsonText", t.submitters AS "submitters: JsonText",
                      t.author_id, t.account_id, t.archived_at, t.created_at, t.updated_at,
                      t.source, t.folder_id, t.external_id,
                      t.preferences AS "preferences: JsonText", t.shared_link,
                      t.variables_schema AS "variables_schema: JsonText"
               FROM templates t WHERE t.slug = $1"#,
        slug
    )
    .fetch_optional(db)
    .await?)
}

pub async fn list(
    db: &PgPool,
    filter: &TemplateFilter<'_>,
    page: Page,
) -> Result<Vec<Template>, DbError> {
    Ok(sqlx::query_as!(
        Template,
        r#"SELECT t.id, t.slug, t.name, t.schema AS "schema: JsonText",
                      t.fields AS "fields: JsonText", t.submitters AS "submitters: JsonText",
                      t.author_id, t.account_id, t.archived_at, t.created_at, t.updated_at,
                      t.source, t.folder_id, t.external_id,
                      t.preferences AS "preferences: JsonText", t.shared_link,
                      t.variables_schema AS "variables_schema: JsonText"
               FROM templates t WHERE ($1::bigint IS NULL OR t.account_id = $1)
                 AND ($2::bigint[] IS NULL OR t.id IN (
                       SELECT s.template_id FROM template_sharings s WHERE s.account_id = ANY($2))
                     AND NOT EXISTS (SELECT 1 FROM template_accesses a WHERE a.template_id = t.id))
                 AND ($3::text IS NULL OR LOWER(t.name) LIKE $3)
                 AND (CASE WHEN $4 THEN t.archived_at IS NOT NULL ELSE t.archived_at IS NULL END)
                 AND ($5::text IS NULL OR t.external_id = $5)
                 AND ($6::text IS NULL OR t.slug = $6)
                 AND ($7::bigint[] IS NULL OR t.folder_id = ANY($7))
                 AND ($8::bigint IS NULL OR t.id < $8)
                 AND ($9::bigint IS NULL OR t.id > $9)
                 AND ($11::bigint[] IS NULL OR t.id = ANY($11))
               ORDER BY t.id DESC
               LIMIT $10"#,
        filter.account_id,
        filter.shared_with,
        filter.name_like,
        filter.archived,
        filter.external_id,
        filter.slug,
        filter.folder_ids,
        page.after,
        page.before,
        page.limit,
        filter.ids,
    )
    .fetch_all(db)
    .await?)
}

pub async fn sharings(
    db: impl PgExecutor<'_>,
    template_id: i64,
) -> Result<Vec<loco_domain::ability::TemplateSharing>, DbError> {
    Ok(sqlx::query_as!(
        loco_domain::ability::TemplateSharing,
        "SELECT account_id, ability FROM template_sharings WHERE template_id = $1 ORDER BY id",
        template_id
    )
    .fetch_all(db)
    .await?)
}

/// The editable columns of `PUT /api/templates/:id`.
#[derive(Clone, Debug)]
pub struct TemplateUpdate {
    pub name: String,
    pub external_id: Option<String>,
    pub shared_link: bool,
    pub submitters: JsonText,
    pub fields: JsonText,
    pub folder_id: i64,
    pub archived_at: Option<NaiveDateTime>,
}

/// Saves the changes and bumps `updated_at` when anything changed, as `update!` does.
/// Returns the new `updated_at`.
pub async fn update(
    db: impl PgExecutor<'_>,
    id: i64,
    changes: &TemplateUpdate,
) -> Result<NaiveDateTime, DbError> {
    Ok(sqlx::query_scalar!(
        r#"UPDATE templates SET
             name = $2, external_id = $3, shared_link = $4, submitters = $5, fields = $6,
             folder_id = $7, archived_at = $8,
             updated_at = CASE
               WHEN (name, external_id, shared_link, submitters, fields, folder_id, archived_at)
                    IS NOT DISTINCT FROM
                    ($2::varchar, $3::varchar, $4::boolean, $5::text, $6::text, $7::bigint, $8::timestamp)
               THEN updated_at ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1
           RETURNING updated_at"#,
        id,
        changes.name,
        changes.external_id,
        changes.shared_link,
        changes.submitters as _,
        changes.fields as _,
        changes.folder_id,
        changes.archived_at,
    )
    .fetch_one(db)
    .await?)
}

/// `update!(archived_at: Time.current)`. Returns the timestamp.
pub async fn archive(db: impl PgExecutor<'_>, id: i64) -> Result<NaiveDateTime, DbError> {
    Ok(sqlx::query_scalar!(
        r#"UPDATE templates SET archived_at = (now() AT TIME ZONE 'utc'),
                                updated_at = (now() AT TIME ZONE 'utc')
           WHERE id = $1 RETURNING archived_at AS "archived_at!""#,
        id
    )
    .fetch_one(db)
    .await?)
}

/// A new template, e.g. a clone (`Templates::Clone`).
#[derive(Clone, Debug)]
pub struct NewTemplate {
    pub name: String,
    pub slug: String,
    pub schema: JsonText,
    pub fields: JsonText,
    pub submitters: JsonText,
    pub author_id: i64,
    pub account_id: i64,
    pub source: String,
    pub folder_id: i64,
    pub external_id: Option<String>,
    pub preferences: JsonText,
    pub shared_link: bool,
    pub variables_schema: Option<JsonText>,
}

pub async fn insert(db: impl PgExecutor<'_>, new: &NewTemplate) -> Result<Template, DbError> {
    Ok(sqlx::query_as!(
        Template,
        r#"INSERT INTO templates
             (name, slug, schema, fields, submitters, author_id, account_id, source, folder_id,
              external_id, preferences, shared_link, variables_schema, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                   (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING id, slug, name, schema AS "schema: JsonText", fields AS "fields: JsonText",
                     submitters AS "submitters: JsonText", author_id, account_id, archived_at,
                     created_at, updated_at, source, folder_id, external_id,
                     preferences AS "preferences: JsonText", shared_link,
                     variables_schema AS "variables_schema: JsonText""#,
        new.name,
        new.slug,
        new.schema as _,
        new.fields as _,
        new.submitters as _,
        new.author_id,
        new.account_id,
        new.source,
        new.folder_id,
        new.external_id,
        new.preferences as _,
        new.shared_link,
        new.variables_schema as _,
    )
    .fetch_one(db)
    .await?)
}

/// Updates the JSON columns a clone rewrites after copying documents.
pub async fn update_documents(
    db: impl PgExecutor<'_>,
    id: i64,
    schema: &JsonText,
    fields: &JsonText,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE templates SET schema = $2, fields = $3 WHERE id = $1",
        id,
        schema as _,
        fields as _,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Copies the template accesses of one template to another (`Templates::Clone`).
pub async fn copy_accesses(db: impl PgExecutor<'_>, from: i64, to: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO template_accesses (template_id, user_id, created_at, updated_at)
           SELECT $2, user_id, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc')
           FROM template_accesses WHERE template_id = $1"#,
        from,
        to,
    )
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub author_id: i64,
    pub account_id: i64,
    pub archived_at: Option<NaiveDateTime>,
    pub parent_folder_id: Option<i64>,
    /// `TemplateFolder#full_name`: `"Parent / Name"` for subfolders.
    pub full_name: String,
}

pub async fn find_folder(db: impl PgExecutor<'_>, id: i64) -> Result<Option<Folder>, DbError> {
    Ok(
        sqlx::query_as!(Folder, r#"SELECT f.id AS "id!", f.name AS "name!", f.author_id AS "author_id!", f.account_id AS "account_id!", f.archived_at, f.parent_folder_id,
                      CASE WHEN p.id IS NULL THEN f.name ELSE p.name || ' / ' || f.name END
                        AS "full_name!"
               FROM template_folders f
               LEFT JOIN template_folders p ON p.id = f.parent_folder_id WHERE f.id = $1"#, id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn find_folders(db: impl PgExecutor<'_>, ids: &[i64]) -> Result<Vec<Folder>, DbError> {
    Ok(
        sqlx::query_as!(Folder, r#"SELECT f.id AS "id!", f.name AS "name!", f.author_id AS "author_id!", f.account_id AS "account_id!", f.archived_at, f.parent_folder_id,
                      CASE WHEN p.id IS NULL THEN f.name ELSE p.name || ' / ' || f.name END
                        AS "full_name!"
               FROM template_folders f
               LEFT JOIN template_folders p ON p.id = f.parent_folder_id WHERE f.id = ANY($1)"#, ids)
            .fetch_all(db)
            .await?,
    )
}

/// Splits `"Parent / Name"` as `TemplateFolders` does: `(parent, name)`, or `(name, None)`
/// for a top-level name. Both parts are squished.
#[must_use]
pub fn split_full_name(full_name: &str) -> (String, Option<String>) {
    let squish = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    match full_name.split_once(" / ") {
        Some((parent, name)) => (squish(parent), Some(squish(name))),
        None => (squish(full_name), None),
    }
}

/// `TemplateFolders.filter_by_full_name` within an account: ids of the matching folders.
pub async fn filter_folders_by_full_name(
    db: &PgPool,
    account_id: i64,
    full_name: &str,
) -> Result<Vec<i64>, DbError> {
    if full_name.trim().is_empty() {
        return Ok(Vec::new());
    }
    let (first, second) = split_full_name(full_name);
    let ids = match second.filter(|n| !n.is_empty()) {
        Some(name) => {
            sqlx::query_scalar!(
                "SELECT f.id FROM template_folders f
                 WHERE f.account_id = $1 AND f.name = $3
                   AND f.parent_folder_id IS NOT DISTINCT FROM (
                     SELECT p.id FROM template_folders p
                     WHERE p.account_id = $1 AND p.parent_folder_id IS NULL AND p.name = $2
                     ORDER BY p.id LIMIT 1)",
                account_id,
                first,
                name,
            )
            .fetch_all(db)
            .await?
        }
        None => {
            sqlx::query_scalar!(
                "SELECT id FROM template_folders
                 WHERE account_id = $1 AND name = $2 AND parent_folder_id IS NULL",
                account_id,
                first,
            )
            .fetch_all(db)
            .await?
        }
    };
    Ok(ids)
}

/// Ids of the direct subfolders of `folder_ids`.
pub async fn subfolder_ids(db: &PgPool, folder_ids: &[i64]) -> Result<Vec<i64>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT id FROM template_folders WHERE parent_folder_id = ANY($1) ORDER BY id",
        folder_ids
    )
    .fetch_all(db)
    .await?)
}

/// `account.default_template_folder`, created with the account's first user as author
/// when missing.
pub async fn default_folder(db: &PgPool, account_id: i64) -> Result<i64, DbError> {
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM template_folders WHERE account_id = $1 AND name = $2 ORDER BY id LIMIT 1",
        account_id,
        DEFAULT_FOLDER_NAME
    )
    .fetch_optional(db)
    .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO template_folders (name, author_id, account_id, created_at, updated_at)
           SELECT $2, MIN(u.id), $1, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc')
           FROM users u WHERE u.account_id = $1
           RETURNING id"#,
        account_id,
        DEFAULT_FOLDER_NAME
    )
    .fetch_one(db)
    .await?)
}

async fn find_or_create_folder(
    db: &PgPool,
    account_id: i64,
    author_id: i64,
    name: &str,
    parent_folder_id: Option<i64>,
) -> Result<i64, DbError> {
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM template_folders
         WHERE account_id = $1 AND name = $2 AND parent_folder_id IS NOT DISTINCT FROM $3
         ORDER BY id LIMIT 1",
        account_id,
        name,
        parent_folder_id,
    )
    .fetch_optional(db)
    .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO template_folders
             (name, author_id, account_id, parent_folder_id, created_at, updated_at)
           VALUES ($1, $2, $3, $4, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING id"#,
        name,
        author_id,
        account_id,
        parent_folder_id,
    )
    .fetch_one(db)
    .await?)
}

/// `TemplateFolders.find_or_create_by_name(author, name)`.
pub async fn find_or_create_folder_by_name(
    db: &PgPool,
    account_id: i64,
    author_id: i64,
    full_name: &str,
) -> Result<i64, DbError> {
    if full_name.trim().is_empty() || full_name == DEFAULT_FOLDER_NAME {
        return default_folder(db, account_id).await;
    }
    let (first, second) = split_full_name(full_name);
    match second.filter(|n| !n.is_empty()) {
        Some(name) => {
            let parent = find_or_create_folder(db, account_id, author_id, &first, None).await?;
            find_or_create_folder(db, account_id, author_id, &name, Some(parent)).await
        }
        None => find_or_create_folder(db, account_id, author_id, &first, None).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_folder_names_like_rails() {
        assert_eq!(split_full_name("Sales"), ("Sales".into(), None));
        assert_eq!(
            split_full_name("  Sales  /  EU   2026 "),
            ("Sales".into(), Some("EU 2026".into()))
        );
        assert_eq!(
            split_full_name("A / B / C"),
            ("A".into(), Some("B / C".into()))
        );
    }
}
