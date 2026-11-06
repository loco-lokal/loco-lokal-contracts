//! Translations from `config/locales/i18n.yml` (the same file Rails uses), with Rails'
//! semantics: nested keys joined with `.`, `%{name}` interpolation, `one`/`other`
//! pluralization by `count`, `_html` keys rendered as markup, and fallbacks
//! `de-DE → de → en`. Like Rails, every `*.yml` in `config/locales/` is loaded in sorted
//! order, later files overriding earlier ones.

use std::{collections::HashMap, path::Path};

use yaml_rust2::{
    Event, Yaml,
    parser::{MarkedEventReceiver, Parser},
    scanner::{Marker, TScalarStyle},
    yaml::Hash,
};

pub const DEFAULT_LOCALE: &str = "en";

/// Locales enabled in `config/application.rb`.
pub const AVAILABLE_LOCALES: &[&str] = &[
    "en", "en-US", "en-GB", "es-ES", "fr-FR", "pt-PT", "de-DE", "it-IT", "nl-NL", "es", "it", "de",
    "fr", "nl", "pl", "uk", "cs", "pt", "he", "ar", "ko", "ja",
];

#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Yaml {
        path: String,
        source: yaml_rust2::ScanError,
    },
}

#[derive(Clone, Debug)]
enum Entry {
    Text(String),
    Plural(HashMap<String, String>),
}

#[derive(Clone, Debug, Default)]
pub struct I18n {
    locales: HashMap<String, HashMap<String, Entry>>,
}

impl I18n {
    /// Loads every `*.yml` file in `dir` (Rails' `config/locales`).
    pub fn load_dir(dir: &Path) -> Result<Self, I18nError> {
        let io_error = |source| I18nError::Io {
            path: dir.display().to_string(),
            source,
        };
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map_err(io_error)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("yml"))
            })
            .collect();
        files.sort();

        let mut i18n = Self::default();
        for file in files {
            i18n.merge(Self::load(&file)?);
        }
        Ok(i18n)
    }

    fn merge(&mut self, other: Self) {
        for (locale, entries) in other.locales {
            self.locales.entry(locale).or_default().extend(entries);
        }
    }

    pub fn load(path: &Path) -> Result<Self, I18nError> {
        let source = std::fs::read_to_string(path).map_err(|source| I18nError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&source).map_err(|source| I18nError::Yaml {
            path: path.display().to_string(),
            source,
        })
    }

    /// Parses YAML the way Ruby's Psych does for this file: duplicate keys keep the last
    /// value, and `<<: *anchor` merges keys the mapping does not define itself.
    pub fn parse(source: &str) -> Result<Self, yaml_rust2::ScanError> {
        let root = load_yaml(source)?;

        let mut locales = HashMap::new();
        if let Yaml::Hash(map) = root {
            for (locale, tree) in map {
                let Some(locale) = locale.as_str() else {
                    continue;
                };
                let mut entries = HashMap::new();
                flatten(&tree, String::new(), &mut entries);
                locales.insert(locale.to_owned(), entries);
            }
        }

        Ok(Self { locales })
    }

    #[must_use]
    pub fn has_locale(&self, locale: &str) -> bool {
        self.locales.contains_key(locale)
    }

    /// Translates `key`, interpolating `vars`. `count` selects a plural form when the entry
    /// is pluralized. Returns `None` when no locale in the fallback chain has the key.
    #[must_use]
    pub fn translate(&self, locale: &str, key: &str, vars: &[(&str, String)]) -> Option<String> {
        let entry = fallback_chain(locale)
            .into_iter()
            .find_map(|l| self.locales.get(l).and_then(|m| m.get(key)))?;

        let template = match entry {
            Entry::Text(text) => text.as_str(),
            Entry::Plural(forms) => {
                let count = vars
                    .iter()
                    .find(|(k, _)| *k == "count")
                    .and_then(|(_, v)| v.parse::<i64>().ok());
                let form = match count {
                    Some(0) if forms.contains_key("zero") => "zero",
                    Some(1) => "one",
                    _ => "other",
                };
                forms
                    .get(form)
                    .or_else(|| forms.get("other"))
                    .map(String::as_str)?
            }
        };

        Some(interpolate(template, vars))
    }
}

/// Loads the first YAML document. Unlike `YamlLoader`, a duplicate key replaces the earlier
/// value instead of failing, matching Ruby's Psych.
fn load_yaml(source: &str) -> Result<Yaml, yaml_rust2::ScanError> {
    #[derive(Default)]
    struct Loader {
        docs: Vec<Yaml>,
        stack: Vec<(Yaml, usize)>,
        keys: Vec<Option<Yaml>>,
        anchors: HashMap<usize, Yaml>,
    }

    impl Loader {
        fn insert(&mut self, node: Yaml, anchor: usize) {
            if anchor > 0 {
                self.anchors.insert(anchor, node.clone());
            }
            match self.stack.last_mut() {
                None => self.docs.push(node),
                Some((Yaml::Array(items), _)) => items.push(node),
                Some((Yaml::Hash(map), _)) => {
                    let key = self.keys.last_mut().expect("key slot for mapping");
                    match key.take() {
                        None => *key = Some(node),
                        Some(k) => {
                            map.insert(k, node);
                        }
                    }
                }
                Some(_) => unreachable!("only collections are pushed"),
            }
        }
    }

    impl MarkedEventReceiver for Loader {
        fn on_event(&mut self, event: Event, _mark: Marker) {
            match event {
                Event::MappingStart(anchor, _) => {
                    self.stack.push((Yaml::Hash(Hash::new()), anchor));
                    self.keys.push(None);
                }
                Event::SequenceStart(anchor, _) => {
                    self.stack.push((Yaml::Array(Vec::new()), anchor));
                }
                Event::MappingEnd => {
                    self.keys.pop();
                    let (node, anchor) = self.stack.pop().expect("open mapping");
                    self.insert(node, anchor);
                }
                Event::SequenceEnd => {
                    let (node, anchor) = self.stack.pop().expect("open sequence");
                    self.insert(node, anchor);
                }
                Event::Scalar(value, style, anchor, _) => {
                    let node = if style == TScalarStyle::Plain {
                        Yaml::from_str(&value)
                    } else {
                        Yaml::String(value)
                    };
                    self.insert(node, anchor);
                }
                Event::Alias(id) => {
                    let node = self.anchors.get(&id).cloned().unwrap_or(Yaml::BadValue);
                    self.insert(node, 0);
                }
                _ => {}
            }
        }
    }

    let mut loader = Loader::default();
    Parser::new_from_str(source).load(&mut loader, false)?;
    Ok(loader.docs.into_iter().next().unwrap_or(Yaml::Null))
}

/// `de-DE` → `["de-DE", "de", "en"]`.
fn fallback_chain(locale: &str) -> Vec<&str> {
    let mut chain = vec![locale];
    if let Some((language, _)) = locale.split_once('-') {
        chain.push(language);
    }
    if !chain.contains(&DEFAULT_LOCALE) {
        chain.push(DEFAULT_LOCALE);
    }
    chain
}

fn flatten(value: &Yaml, prefix: String, out: &mut HashMap<String, Entry>) {
    match value {
        Yaml::Hash(map) => {
            let merged;
            let map = if let Some(base) = map.get(&Yaml::String("<<".into())) {
                let mut combined = match base {
                    Yaml::Hash(base) => base.clone(),
                    _ => yaml_rust2::yaml::Hash::new(),
                };
                for (k, v) in map {
                    if k.as_str() != Some("<<") {
                        combined.insert(k.clone(), v.clone());
                    }
                }
                merged = combined;
                &merged
            } else {
                map
            };

            let is_plural = !map.is_empty()
                && map.iter().all(|(k, v)| {
                    matches!(k.as_str(), Some("zero" | "one" | "few" | "many" | "other"))
                        && scalar(v).is_some()
                });

            if is_plural {
                let forms = map
                    .iter()
                    .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), scalar(v)?)))
                    .collect();
                out.insert(prefix, Entry::Plural(forms));
                return;
            }

            for (k, v) in map {
                let Some(k) = scalar(k) else { continue };
                let key = if prefix.is_empty() {
                    k
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(v, key, out);
            }
        }
        other => {
            if let Some(text) = scalar(other) {
                out.insert(prefix, Entry::Text(text));
            }
        }
    }
}

fn scalar(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(n) => Some(n.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn interpolate(template: &str, vars: &[(&str, String)]) -> String {
    if !template.contains("%{") {
        return template.to_owned();
    }

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let name = &after[..end];
            match vars.iter().find(|(k, _)| *k == name) {
                Some((_, value)) => out.push_str(value),
                None => out.push_str(&rest[start..start + 3 + end]),
            }
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r"
en: &en
  hello_name: Hi %{name}
  agency:
    page_title: Loco Lokal Contracts
  documents:
    one: '%{count} document'
    other: '%{count} documents'
de: &de
  hello_name: Hallo %{name}
de-DE:
  <<: *de
";

    #[test]
    fn nested_keys_interpolation_and_fallbacks() {
        let i18n = I18n::parse(SAMPLE).unwrap();
        let name = [("name", "Ana".to_owned())];

        assert_eq!(i18n.translate("en", "hello_name", &name).unwrap(), "Hi Ana");
        assert_eq!(
            i18n.translate("de-DE", "hello_name", &name).unwrap(),
            "Hallo Ana"
        );
        // Missing in de-DE and de, falls back to en.
        assert_eq!(
            i18n.translate("de-DE", "agency.page_title", &[]).unwrap(),
            "Loco Lokal Contracts"
        );
        assert!(i18n.translate("en", "missing", &[]).is_none());
    }

    #[test]
    fn pluralizes_by_count() {
        let i18n = I18n::parse(SAMPLE).unwrap();
        let t = |n: &str| {
            i18n.translate("en", "documents", &[("count", n.to_owned())])
                .unwrap()
        };

        assert_eq!(t("1"), "1 document");
        assert_eq!(t("3"), "3 documents");
    }

    #[test]
    fn loads_the_rails_locale_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/locales");
        let i18n = I18n::load_dir(&dir).unwrap();

        for locale in AVAILABLE_LOCALES {
            assert!(i18n.has_locale(locale), "missing locale {locale}");
        }
        assert_eq!(
            i18n.translate("en-GB", "agency.page_title", &[]).as_deref(),
            Some("Loco Lokal | Contracts")
        );
        assert_eq!(
            i18n.translate("de-DE", "agency.page_title", &[]).as_deref(),
            Some("Loco Lokal | Verträge")
        );
        assert_eq!(
            i18n.translate("de-DE", "hello_name", &[("name", "Ana".into())])
                .as_deref(),
            i18n.translate("de", "hello_name", &[("name", "Ana".into())])
                .as_deref()
        );
    }
}
