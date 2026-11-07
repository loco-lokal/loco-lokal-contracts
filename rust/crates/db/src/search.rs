//! `SearchEntries`: the full-text index (`search_entries`) and the queries over it. The
//! query text is built as Rails builds it, so both backends find the same records.

use std::{
    fmt::Write as _,
    sync::{LazyLock, OnceLock},
};

use regex::Regex;
use serde_json::Value;
use sqlx::{PgExecutor, PgPool};

use crate::DbError;

/// `SearchEntries::MAX_VALUE_LENGTH`.
const MAX_VALUE_LENGTH: usize = 100;

/// `TextUtils::TRANSLITERATIONS`: I18n's approximations that map to one character.
const TRANSLITERATIONS: &[(char, char)] = &[
    ('À', 'A'),
    ('Á', 'A'),
    ('Â', 'A'),
    ('Ã', 'A'),
    ('Ä', 'A'),
    ('Å', 'A'),
    ('Ç', 'C'),
    ('È', 'E'),
    ('É', 'E'),
    ('Ê', 'E'),
    ('Ë', 'E'),
    ('Ì', 'I'),
    ('Í', 'I'),
    ('Î', 'I'),
    ('Ï', 'I'),
    ('Ð', 'D'),
    ('Ñ', 'N'),
    ('Ò', 'O'),
    ('Ó', 'O'),
    ('Ô', 'O'),
    ('Õ', 'O'),
    ('Ö', 'O'),
    ('×', 'x'),
    ('Ø', 'O'),
    ('Ù', 'U'),
    ('Ú', 'U'),
    ('Û', 'U'),
    ('Ü', 'U'),
    ('Ý', 'Y'),
    ('à', 'a'),
    ('á', 'a'),
    ('â', 'a'),
    ('ã', 'a'),
    ('ä', 'a'),
    ('å', 'a'),
    ('ç', 'c'),
    ('è', 'e'),
    ('é', 'e'),
    ('ê', 'e'),
    ('ë', 'e'),
    ('ì', 'i'),
    ('í', 'i'),
    ('î', 'i'),
    ('ï', 'i'),
    ('ð', 'd'),
    ('ñ', 'n'),
    ('ò', 'o'),
    ('ó', 'o'),
    ('ô', 'o'),
    ('õ', 'o'),
    ('ö', 'o'),
    ('ø', 'o'),
    ('ù', 'u'),
    ('ú', 'u'),
    ('û', 'u'),
    ('ü', 'u'),
    ('ý', 'y'),
    ('ÿ', 'y'),
    ('Ā', 'A'),
    ('ā', 'a'),
    ('Ă', 'A'),
    ('ă', 'a'),
    ('Ą', 'A'),
    ('ą', 'a'),
    ('Ć', 'C'),
    ('ć', 'c'),
    ('Ĉ', 'C'),
    ('ĉ', 'c'),
    ('Ċ', 'C'),
    ('ċ', 'c'),
    ('Č', 'C'),
    ('č', 'c'),
    ('Ď', 'D'),
    ('ď', 'd'),
    ('Đ', 'D'),
    ('đ', 'd'),
    ('Ē', 'E'),
    ('ē', 'e'),
    ('Ĕ', 'E'),
    ('ĕ', 'e'),
    ('Ė', 'E'),
    ('ė', 'e'),
    ('Ę', 'E'),
    ('ę', 'e'),
    ('Ě', 'E'),
    ('ě', 'e'),
    ('Ĝ', 'G'),
    ('ĝ', 'g'),
    ('Ğ', 'G'),
    ('ğ', 'g'),
    ('Ġ', 'G'),
    ('ġ', 'g'),
    ('Ģ', 'G'),
    ('ģ', 'g'),
    ('Ĥ', 'H'),
    ('ĥ', 'h'),
    ('Ħ', 'H'),
    ('ħ', 'h'),
    ('Ĩ', 'I'),
    ('ĩ', 'i'),
    ('Ī', 'I'),
    ('ī', 'i'),
    ('Ĭ', 'I'),
    ('ĭ', 'i'),
    ('Į', 'I'),
    ('į', 'i'),
    ('İ', 'I'),
    ('ı', 'i'),
    ('Ĵ', 'J'),
    ('ĵ', 'j'),
    ('Ķ', 'K'),
    ('ķ', 'k'),
    ('ĸ', 'k'),
    ('Ĺ', 'L'),
    ('ĺ', 'l'),
    ('Ļ', 'L'),
    ('ļ', 'l'),
    ('Ľ', 'L'),
    ('ľ', 'l'),
    ('Ŀ', 'L'),
    ('ŀ', 'l'),
    ('Ł', 'L'),
    ('ł', 'l'),
    ('Ń', 'N'),
    ('ń', 'n'),
    ('Ņ', 'N'),
    ('ņ', 'n'),
    ('Ň', 'N'),
    ('ň', 'n'),
    ('Ō', 'O'),
    ('ō', 'o'),
    ('Ŏ', 'O'),
    ('ŏ', 'o'),
    ('Ő', 'O'),
    ('ő', 'o'),
    ('Ŕ', 'R'),
    ('ŕ', 'r'),
    ('Ŗ', 'R'),
    ('ŗ', 'r'),
    ('Ř', 'R'),
    ('ř', 'r'),
    ('Ś', 'S'),
    ('ś', 's'),
    ('Ŝ', 'S'),
    ('ŝ', 's'),
    ('Ş', 'S'),
    ('ş', 's'),
    ('Š', 'S'),
    ('š', 's'),
    ('Ţ', 'T'),
    ('ţ', 't'),
    ('Ť', 'T'),
    ('ť', 't'),
    ('Ŧ', 'T'),
    ('ŧ', 't'),
    ('Ũ', 'U'),
    ('ũ', 'u'),
    ('Ū', 'U'),
    ('ū', 'u'),
    ('Ŭ', 'U'),
    ('ŭ', 'u'),
    ('Ů', 'U'),
    ('ů', 'u'),
    ('Ű', 'U'),
    ('ű', 'u'),
    ('Ų', 'U'),
    ('ų', 'u'),
    ('Ŵ', 'W'),
    ('ŵ', 'w'),
    ('Ŷ', 'Y'),
    ('ŷ', 'y'),
    ('Ÿ', 'Y'),
    ('Ź', 'Z'),
    ('ź', 'z'),
    ('Ż', 'Z'),
    ('ż', 'z'),
    ('Ž', 'Z'),
    ('ž', 'z'),
    ('Ǫ', 'O'),
    ('ǫ', 'o'),
    ('Ǭ', 'O'),
    ('ǭ', 'o'),
];

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\A[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\z")
        .expect("valid regex")
});

/// `PhoneCodes::REGEXP`. Only the first alternative is anchored, as in Rails.
static PHONE_CODES: LazyLock<Regex> = LazyLock::new(|| {
    let codes: Vec<String> = include_str!("phone_codes.txt")
        .lines()
        .map(regex::escape)
        .collect();
    Regex::new(&format!(r"(?i)\A{}", codes.join("|"))).expect("valid regex")
});

static HYPHENS_BEFORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)\S*?\d-\S+?(?-u:\b)").expect("valid regex"));
static HYPHENS_AFTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)\S+-\d\S*?(?-u:\b)").expect("valid regex"));
static NGRAM_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s'").expect("valid regex"));
static NGRAM_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"'(.*?)':").expect("valid regex"));
static NGRAM_POSITION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"':[\d,]").expect("valid regex"));

/// `TextUtils.transliterate`.
#[must_use]
pub fn transliterate(text: &str) -> String {
    text.chars()
        .map(|c| {
            TRANSLITERATIONS
                .iter()
                .find(|(from, _)| *from == c)
                .map_or(c, |(_, to)| *to)
        })
        .collect()
}

/// `String#squish`.
#[must_use]
pub fn squish(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `Docuseal.fulltext_search?`: an account turned full-text search on.
pub async fn enabled(db: impl PgExecutor<'_>) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM account_configs
                          WHERE key = 'fulltext_search' AND value = 'true') AS "exists!""#
    )
    .fetch_one(db)
    .await?)
}

/// `SearchEntries.default_dictionary`: the first dictionary of the current text search
/// configuration, as an SQL literal.
async fn dictionary(db: &PgPool) -> Result<&'static str, DbError> {
    static DICTIONARY: OnceLock<String> = OnceLock::new();
    if let Some(d) = DICTIONARY.get() {
        return Ok(d);
    }
    let name: Option<String> = sqlx::query_scalar(
        "SELECT mapdict::regdictionary::text FROM pg_ts_config_map
         WHERE mapcfg = get_current_ts_config() AND maptokentype = 1
         ORDER BY mapseqno LIMIT 1",
    )
    .fetch_optional(db)
    .await?;
    let name = name.unwrap_or_else(|| "simple".to_owned());
    Ok(DICTIONARY.get_or_init(|| format!("'{}'", name.replace('\'', "''"))))
}

/// A condition over `search_entries` and its text parameters.
#[derive(Debug, PartialEq, Eq)]
pub struct TsQuery {
    /// SQL with `$n` placeholders numbered from the `first` given to [`build_tsquery`].
    pub sql: String,
    pub binds: Vec<String>,
}

/// `SearchEntries.build_tsquery`. `dictionary` is an SQL literal.
#[must_use]
pub fn build_tsquery(
    keyword: &str,
    with_or_vector: bool,
    dictionary: &str,
    first: usize,
) -> TsQuery {
    let keyword = keyword.replace('\0', "");
    let p = |i: usize| format!("${}", first + i);
    let has_digit = keyword.chars().any(|c| c.is_ascii_digit());
    let has_letter = keyword.chars().any(char::is_alphabetic);

    if has_digit && !has_letter {
        let number: String = keyword.chars().filter(char::is_ascii_digit).collect();
        let short = if number.chars().count() > 1 {
            number.strip_prefix('0').unwrap_or(&number).to_owned()
        } else {
            number.clone()
        };
        let sql = if number.chars().count() <= 2 {
            format!(
                "ngram @@ (quote_literal({})::tsquery || quote_literal({})::tsquery) OR tsvector @@ plainto_tsquery({})",
                p(0),
                p(1),
                p(2)
            )
        } else {
            format!(
                "tsvector @@ ((quote_literal({}) || ':*')::tsquery || (quote_literal({}) || ':*')::tsquery || plainto_tsquery({}))",
                p(0),
                p(1),
                p(2)
            )
        };
        return TsQuery {
            sql,
            binds: vec![number, short, keyword],
        };
    }

    let quoted = keyword.chars().count() >= 2
        && keyword.starts_with(['\'', '"'])
        && keyword.ends_with(['\'', '"'])
        && !keyword.contains('\n');
    let has_other = keyword
        .chars()
        .any(|c| !(c.is_alphabetic() || c.is_ascii_digit() || matches!(c, '&' | '@' | '.' | '-')));
    let repeated = keyword
        .as_bytes()
        .windows(2)
        .any(|w| matches!(w[0], b'.' | b'-') && matches!(w[1], b'.' | b'-'));
    if keyword.starts_with('@') || has_other || quoted || repeated {
        return TsQuery {
            sql: format!("tsvector @@ plainto_tsquery({})", p(0)),
            binds: vec![transliterate(&keyword.to_lowercase())],
        };
    }

    let keyword = squish(&transliterate(&keyword.to_lowercase()));
    let sql = if keyword.chars().count() <= 2 {
        if with_or_vector {
            format!(
                "(ngram @@ quote_literal({0})::tsquery OR tsvector @@ plainto_tsquery({0}))",
                p(0)
            )
        } else {
            format!("ngram @@ quote_literal({})::tsquery", p(0))
        }
    } else {
        format!(
            "tsvector @@ (quote_literal(coalesce((ts_lexize({dictionary}, {0}))[1], {0})) || ':*')::tsquery",
            p(0)
        )
    };
    TsQuery {
        sql,
        binds: vec![keyword],
    }
}

/// Ids of the `record_type` records of `account_ids` whose entry matches `keyword`.
pub async fn matching_ids(
    db: &PgPool,
    record_type: &str,
    account_ids: &[i64],
    keyword: &str,
    with_or_vector: bool,
) -> Result<Vec<i64>, DbError> {
    let dictionary = dictionary(db).await?;
    let query = build_tsquery(keyword, with_or_vector, dictionary, 3);
    let sql = format!(
        "SELECT record_id FROM search_entries
         WHERE record_type = $1 AND account_id = ANY($2) AND ({})",
        query.sql
    );
    // The SQL text is built from fixed fragments; the keyword is bound.
    let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
        .bind(record_type)
        .bind(account_ids);
    for bind in &query.binds {
        q = q.bind(bind);
    }
    Ok(q.fetch_all(db).await?)
}

/// `Submissions.fulltext_search` (without the template search): submissions whose name
/// matches, or with a matching submitter.
pub async fn matching_submission_ids(
    db: &PgPool,
    account_id: i64,
    keyword: &str,
) -> Result<Vec<i64>, DbError> {
    let mut ids = matching_ids(db, "Submission", &[account_id], keyword, false).await?;
    let submitters = matching_ids(db, "Submitter", &[account_id], keyword, true).await?;
    if !submitters.is_empty() {
        ids.extend(
            sqlx::query_scalar!(
                "SELECT submission_id FROM submitters WHERE id = ANY($1)",
                &submitters
            )
            .fetch_all(db)
            .await?,
        );
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// `SearchEntries.add_hyphens`: keeps `12-34`-style words whole.
fn add_hyphens(tsvector: &mut String, text: &str) {
    let text = text.replace('\\', " ");
    let mut items: Vec<&str> = HYPHENS_BEFORE
        .find_iter(&text)
        .chain(HYPHENS_AFTER.find_iter(&text))
        .map(|m| m.as_str())
        .collect();
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(*i));
    for (index, item) in items.iter().enumerate() {
        if !tsvector.contains(item) {
            let _ = write!(tsvector, " '{}':{}", item.replace('\'', ""), index + 1);
        }
    }
}

/// `SearchEntries.build_ngram`: each lexeme's first two and first one characters.
#[must_use]
pub fn build_ngram(ngram: &str) -> String {
    let mut items = Vec::new();
    let mut start = 0;
    for m in NGRAM_SPLIT.find_iter(ngram) {
        items.push(&ngram[start..m.start()]);
        start = m.start() + m.len() - 1;
    }
    items.push(&ngram[start..]);

    let mut out: Vec<String> = Vec::new();
    let mut keys = std::collections::HashSet::new();
    for item in items {
        for n in [2, 1] {
            let gram = NGRAM_WORD
                .replacen(item, 1, |c: &regex::Captures<'_>| {
                    format!("'{}':", c[1].chars().take(n).collect::<String>())
                })
                .into_owned();
            let key = NGRAM_POSITION.replacen(&gram, 1, "':1").into_owned();
            if keys.insert(key) {
                out.push(gram);
            }
        }
    }
    out.join(" ")
}

async fn save(
    db: &PgPool,
    record_type: &str,
    record_id: i64,
    account_id: i64,
    vectors: (String, String),
    hyphen_text: &str,
) -> Result<(), DbError> {
    let (mut tsvector, ngram) = vectors;
    add_hyphens(&mut tsvector, hyphen_text);
    if tsvector.trim().is_empty() {
        return Ok(());
    }
    sqlx::query!(
        r#"INSERT INTO search_entries
             (record_type, record_id, account_id, tsvector, ngram, created_at, updated_at)
           VALUES ($1, $2, $3, $4::text::tsvector, $5::text::tsvector,
                   (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           ON CONFLICT (record_id, record_type) DO UPDATE SET
             account_id = EXCLUDED.account_id, tsvector = EXCLUDED.tsvector,
             ngram = EXCLUDED.ngram,
             updated_at = CASE
               WHEN (search_entries.account_id, search_entries.tsvector, search_entries.ngram)
                    IS NOT DISTINCT FROM (EXCLUDED.account_id, EXCLUDED.tsvector, EXCLUDED.ngram)
               THEN search_entries.updated_at ELSE EXCLUDED.updated_at END"#,
        record_type,
        record_id,
        account_id,
        tsvector,
        build_ngram(&ngram),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `index_template` / `index_submission`: one weighted vector of the name.
async fn index_name(
    db: &PgPool,
    record_type: &str,
    record_id: i64,
    account_id: i64,
    name: &str,
) -> Result<(), DbError> {
    let text = transliterate(&squish(&name.to_lowercase())).replace('\0', "");
    let vectors: (String, String) =
        sqlx::query_as("SELECT to_tsvector($1)::text, to_tsvector('simple', $1)::text")
            .bind(&text)
            .fetch_one(db)
            .await?;
    save(db, record_type, record_id, account_id, vectors, &text).await
}

fn digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

/// `build_submitter_values_string`.
fn submitter_values_string(values: &Value) -> String {
    fn flatten<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
        match value {
            Value::Array(items) => items.iter().for_each(|v| flatten(v, out)),
            other => out.push(other),
        }
    }
    let mut flat = Vec::new();
    if let Value::Object(map) = values {
        map.values().for_each(|v| flatten(v, &mut flat));
    }
    let mut strings: Vec<String> = Vec::new();
    for v in flat {
        let Value::String(s) = v else { continue };
        if s.chars().count() > MAX_VALUE_LENGTH || UUID.is_match(s) {
            continue;
        }
        let s = transliterate(s);
        if !strings.contains(&s) {
            strings.push(s);
        }
    }
    strings.join(" ").to_lowercase().replace('\0', "")
}

/// `SearchEntries.index_submitter`.
async fn index_submitter(db: &PgPool, id: i64) -> Result<(), DbError> {
    let Some(r) = sqlx::query!(
        r#"SELECT account_id, email, phone, name, values FROM submitters WHERE id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?
    else {
        return Ok(());
    };
    let blank = |s: &Option<String>| s.as_deref().is_none_or(|s| s.trim().is_empty());
    if blank(&r.email) && blank(&r.phone) && blank(&r.name) {
        return Ok(());
    }
    let email = r.email.unwrap_or_default();
    let domain = email.rsplit('@').next().unwrap_or_default();
    let domain = if email.is_empty() { "" } else { domain };
    let email = format!("{email} {domain}").replace('\0', "");
    let phone = r.phone.unwrap_or_default();
    let mut phones = vec![digits(&phone)];
    let local = digits(&PHONE_CODES.replace_all(&phone, ""));
    if !phones.contains(&local) {
        phones.push(local);
    }
    let phone = phones.join(" ").replace('\0', "");
    let name = transliterate(&r.name.unwrap_or_default()).replace('\0', "");
    let values: Value = serde_json::from_str(&r.values).unwrap_or(Value::Null);
    let values = submitter_values_string(&values);

    let vectors: (String, String) = sqlx::query_as(
        "SELECT (setweight(to_tsvector($1), 'A') || setweight(to_tsvector($2), 'B') ||
                 setweight(to_tsvector($3), 'C') || setweight(to_tsvector($4), 'D'))::text,
                (setweight(to_tsvector('simple', $1), 'A') ||
                 setweight(to_tsvector('simple', $2), 'B') ||
                 setweight(to_tsvector('simple', $3), 'C'))::text",
    )
    .bind(&email)
    .bind(&phone)
    .bind(&name)
    .bind(&values)
    .fetch_one(db)
    .await?;
    save(db, "Submitter", id, r.account_id, vectors, &values).await
}

/// `SearchEntries.reindex_record`. A submission reindexes its submitters too.
pub async fn reindex(db: &PgPool, record_type: &str, id: i64) -> Result<(), DbError> {
    match record_type {
        "Template" => {
            let row = sqlx::query!("SELECT account_id, name FROM templates WHERE id = $1", id)
                .fetch_optional(db)
                .await?;
            if let Some(row) = row {
                index_name(db, "Template", id, row.account_id, &row.name).await?;
            }
        }
        "Submission" => {
            let row = sqlx::query!("SELECT account_id, name FROM submissions WHERE id = $1", id)
                .fetch_optional(db)
                .await?;
            if let Some(row) = row
                && let Some(name) = row.name.filter(|n| !n.trim().is_empty())
            {
                index_name(db, "Submission", id, row.account_id, &name).await?;
            }
            let submitters = sqlx::query_scalar!(
                "SELECT id FROM submitters WHERE submission_id = $1 ORDER BY id",
                id
            )
            .fetch_all(db)
            .await?;
            for submitter in submitters {
                index_submitter(db, submitter).await?;
            }
        }
        "Submitter" => index_submitter(db, id).await?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DICT: &str = "'english_stem'";

    #[test]
    fn builds_queries_like_rails() {
        let q = build_tsquery("Parity", false, DICT, 3);
        assert_eq!(
            q.sql,
            "tsvector @@ (quote_literal(coalesce((ts_lexize('english_stem', $3))[1], $3)) || ':*')::tsquery"
        );
        assert_eq!(q.binds, ["parity"]);

        let q = build_tsquery("ab", true, DICT, 1);
        assert_eq!(
            q.sql,
            "(ngram @@ quote_literal($1)::tsquery OR tsvector @@ plainto_tsquery($1))"
        );

        let q = build_tsquery("0123", false, DICT, 1);
        assert_eq!(q.binds, ["0123", "123", "0123"]);
        assert!(
            q.sql
                .starts_with("tsvector @@ ((quote_literal($1) || ':*')")
        );

        let q = build_tsquery("Jöhn Doe", false, DICT, 1);
        assert_eq!(q.sql, "tsvector @@ plainto_tsquery($1)");
        assert_eq!(q.binds, ["john doe"]);

        assert_eq!(
            build_tsquery("a--b", false, DICT, 1).sql,
            "tsvector @@ plainto_tsquery($1)"
        );
        assert_eq!(
            build_tsquery("@x", false, DICT, 1).sql,
            "tsvector @@ plainto_tsquery($1)"
        );
    }

    #[test]
    fn builds_ngrams() {
        assert_eq!(build_ngram("'doe':2 'john':1"), "'do':2 'd':2 'jo':1 'j':1");
        assert_eq!(build_ngram("'ab':1 'abc':2"), "'ab':1 'a':1");
    }

    #[test]
    fn strips_phone_codes() {
        assert_eq!(
            digits(&PHONE_CODES.replace_all("+49 170 1234", "")),
            "1701234"
        );
        assert_eq!(digits(&PHONE_CODES.replace_all("+1684 555", "")), "684555");
    }

    #[test]
    fn transliterates() {
        assert_eq!(transliterate("Ärger ß"), "Arger ß");
    }

    /// Reindexes the records Rails indexed in the dev database and compares the entries.
    /// Run with `cargo test -p loco-db -- --ignored reindex_matches_rails`.
    #[tokio::test]
    #[ignore = "needs the Rails-seeded dev database"]
    async fn reindex_matches_rails() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        let select = "SELECT record_type, record_id, account_id, tsvector::text, ngram::text
                      FROM search_entries ORDER BY record_type, record_id";
        let before: Vec<(String, i64, i64, String, Option<String>)> =
            sqlx::query_as(select).fetch_all(&db).await.unwrap();
        for (record_type, id, ..) in &before {
            reindex(&db, record_type, *id).await.unwrap();
        }
        let after: Vec<(String, i64, i64, String, Option<String>)> =
            sqlx::query_as(select).fetch_all(&db).await.unwrap();
        assert_eq!(before, after);
    }
}
