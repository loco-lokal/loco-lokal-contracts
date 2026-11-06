//! Server-side rendering: minijinja templates ported from `app/views`, translations,
//! Shakapacker asset tags, and SVG icons.
//!
//! Templates are loaded from `rust/templates` (`.html` files are auto-escaped). They get
//! these helpers, mirroring the Rails view helpers:
//!
//! - `t(key, **vars)`: translation in the render context's `locale`
//! - `svg_icon(name, class="")`
//! - `javascript_pack_tag(entry, defer=true)`, `stylesheet_pack_tag(entry, media="all", turbo_track=none)`
//! - `csrf_meta_tags()`: uses `csrf_token` from the render context

pub mod assets;
pub mod i18n;
pub mod icons;

use std::{path::PathBuf, sync::Arc};

use minijinja::{Environment, Error, ErrorKind, State, Value, value::Kwargs};
use serde::Serialize;

pub use assets::Assets;
pub use i18n::I18n;
pub use icons::Icons;

#[derive(Clone, Debug)]
pub struct ViewsConfig {
    pub templates_dir: PathBuf,
    pub icons_dir: PathBuf,
    /// Rails' `config/locales`; every `*.yml` in it is loaded.
    pub locales_dir: PathBuf,
    pub manifest_file: PathBuf,
    /// Re-read templates, icons, and the asset manifest when they change (development).
    pub reload: bool,
}

impl ViewsConfig {
    /// Paths relative to the repository root (the Rails app directory).
    #[must_use]
    pub fn from_repo_root(root: &std::path::Path, reload: bool) -> Self {
        Self {
            templates_dir: root.join("rust/templates"),
            icons_dir: root.join("app/views/icons"),
            locales_dir: root.join("config/locales"),
            manifest_file: root.join("public/packs/manifest.json"),
            reload,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ViewsError {
    #[error(transparent)]
    I18n(#[from] i18n::I18nError),
    #[error(transparent)]
    Render(#[from] Error),
}

#[derive(Clone)]
pub struct Views {
    env: Arc<Environment<'static>>,
    i18n: Arc<I18n>,
}

impl Views {
    pub fn new(config: &ViewsConfig) -> Result<Self, ViewsError> {
        let i18n = Arc::new(I18n::load_dir(&config.locales_dir)?);
        let assets = Arc::new(Assets::new(config.manifest_file.clone(), config.reload));
        let icons = Arc::new(Icons::new(config.icons_dir.clone(), config.reload));

        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(config.templates_dir.clone()));
        env.set_auto_reload(config.reload);
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Chainable);
        // Escape like ERB (`& < > " '` only) and print nil/undefined as an empty string,
        // so output can be compared with the Rails pages.
        env.set_formatter(|out, state, value| {
            if *state.auto_escape() == minijinja::AutoEscape::None {
                return minijinja::escape_formatter(out, state, value);
            }
            if value.is_undefined() || value.is_none() {
                return Ok(());
            }
            let text = value.to_string();
            let text = if value.is_safe() {
                text
            } else {
                assets::escape_attr(&text)
            };
            out.write_str(&text).map_err(Error::from)
        });

        let t_i18n = i18n.clone();
        env.add_function("t", move |state: &State, key: &str, kwargs: Kwargs| {
            translate(&t_i18n, state, key, &kwargs)
        });

        env.add_function(
            "svg_icon",
            move |name: &str, kwargs: Kwargs| -> Result<Value, Error> {
                let class: Option<String> = kwargs.get("class")?;
                let locals: Vec<(String, String)> = kwargs
                    .args()
                    .filter(|k| *k != "class")
                    .map(|k| Ok((k.to_owned(), kwargs.get::<Value>(k)?.to_string())))
                    .collect::<Result<_, Error>>()?;
                kwargs.assert_all_used()?;
                let locals: Vec<(&str, String)> = locals
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone()))
                    .collect();

                icons
                    .render(name, class.as_deref().unwrap_or_default(), &locals)
                    .map(Value::from_safe_string)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::InvalidOperation,
                            format!("unknown icon {name:?}"),
                        )
                    })
            },
        );

        let js_assets = assets.clone();
        env.add_function(
            "javascript_pack_tag",
            move |entry: &str, kwargs: Kwargs| -> Result<Value, Error> {
                let defer: Option<bool> = kwargs.get("defer")?;
                kwargs.assert_all_used()?;
                Ok(Value::from_safe_string(
                    js_assets.javascript_tags(entry, defer.unwrap_or(true)),
                ))
            },
        );

        env.add_function(
            "stylesheet_pack_tag",
            move |entry: &str, kwargs: Kwargs| -> Result<Value, Error> {
                let media: Option<String> = kwargs.get("media")?;
                let turbo_track: Option<String> = kwargs.get("turbo_track")?;
                kwargs.assert_all_used()?;
                Ok(Value::from_safe_string(assets.stylesheet_tags(
                    entry,
                    media.as_deref().unwrap_or("all"),
                    turbo_track.as_deref(),
                )))
            },
        );

        env.add_function("csrf_meta_tags", |state: &State| -> Value {
            let token = state
                .lookup("csrf_token")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            Value::from_safe_string(format!(
                "<meta name=\"csrf-param\" content=\"authenticity_token\" />\n<meta name=\"csrf-token\" content=\"{}\" />",
                assets::escape_attr(&token)
            ))
        });

        Ok(Self {
            env: Arc::new(env),
            i18n,
        })
    }

    pub fn render(&self, name: &str, ctx: impl Serialize) -> Result<String, ViewsError> {
        Ok(self
            .env
            .get_template(name)?
            .render(Value::from(minijinja::value::Serde(ctx)))?)
    }

    #[must_use]
    pub fn i18n(&self) -> &I18n {
        &self.i18n
    }

    /// Translation outside templates (e.g. flash messages, email subjects).
    #[must_use]
    pub fn t(&self, locale: &str, key: &str, vars: &[(&str, String)]) -> String {
        self.i18n
            .translate(locale, key, vars)
            .unwrap_or_else(|| missing_translation(key))
    }
}

fn translate(i18n: &I18n, state: &State, key: &str, kwargs: &Kwargs) -> Result<Value, Error> {
    let locale = state
        .lookup("locale")
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| i18n::DEFAULT_LOCALE.to_owned());
    let is_html = key.ends_with("_html") || key.rsplit('.').next() == Some("html");

    let vars = kwargs
        .args()
        .map(|k| {
            let value: Value = kwargs.get(k)?;
            // `_html` translations are markup: escape interpolated values unless they are
            // already safe, as Rails does.
            let text = if is_html && !value.is_safe() {
                assets::escape_attr(&value.to_string())
            } else {
                value.to_string()
            };
            Ok((k, text))
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let text = i18n.translate(&locale, key, &vars).unwrap_or_else(|| {
        tracing::warn!(key, locale, "translation missing");
        missing_translation(key)
    });

    Ok(if is_html {
        Value::from_safe_string(text)
    } else {
        Value::from(text)
    })
}

/// Rails shows the humanized last key segment when a translation is missing.
fn missing_translation(key: &str) -> String {
    let last = key.rsplit('.').next().unwrap_or(key).replace('_', " ");
    let mut chars = last.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
