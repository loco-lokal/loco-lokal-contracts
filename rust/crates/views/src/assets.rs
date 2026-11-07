//! Shakapacker manifest lookups (`public/packs/manifest.json`), producing the same tags as
//! `javascript_pack_tag` and `stylesheet_pack_tag`.

use std::{path::PathBuf, sync::RwLock};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default)]
    entrypoints: std::collections::HashMap<String, Entrypoint>,
}

#[derive(Debug, Default, Deserialize)]
struct Entrypoint {
    #[serde(default)]
    assets: EntrypointAssets,
}

#[derive(Debug, Default, Deserialize)]
struct EntrypointAssets {
    #[serde(default)]
    js: Vec<String>,
    #[serde(default)]
    css: Vec<String>,
}

/// Reads the manifest lazily; with `reload`, re-reads it on every lookup so a running
/// `webpack --watch` or dev server is picked up.
pub struct Assets {
    path: PathBuf,
    reload: bool,
    cached: RwLock<Option<std::sync::Arc<Manifest>>>,
}

impl Assets {
    #[must_use]
    pub fn new(path: PathBuf, reload: bool) -> Self {
        Self {
            path,
            reload,
            cached: RwLock::new(None),
        }
    }

    fn manifest(&self) -> std::sync::Arc<Manifest> {
        if !self.reload
            && let Some(m) = self.cached.read().expect("lock").as_ref()
        {
            return m.clone();
        }

        let manifest = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
            .unwrap_or_else(|| {
                tracing::warn!(path = %self.path.display(), "asset manifest missing or invalid; run the asset build");
                Manifest::default()
            });
        let manifest = std::sync::Arc::new(manifest);
        *self.cached.write().expect("lock") = Some(manifest.clone());
        manifest
    }

    /// `<script>` tags for an entrypoint's chunks.
    #[must_use]
    pub fn javascript_tags(&self, entry: &str, defer: bool) -> String {
        let manifest = self.manifest();
        let Some(entrypoint) = manifest.entrypoints.get(entry) else {
            return String::new();
        };
        let defer = if defer { " defer=\"defer\"" } else { "" };

        entrypoint
            .assets
            .js
            .iter()
            .map(|src| format!("<script src=\"{}\"{defer}></script>", escape_attr(src)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `<link rel="stylesheet">` tags for an entrypoint's CSS chunks.
    #[must_use]
    pub fn stylesheet_tags(&self, entry: &str, media: &str, turbo_track: Option<&str>) -> String {
        let manifest = self.manifest();
        let Some(entrypoint) = manifest.entrypoints.get(entry) else {
            return String::new();
        };
        let track = turbo_track
            .map(|t| format!(" data-turbo-track=\"{}\"", escape_attr(t)))
            .unwrap_or_default();

        entrypoint
            .assets
            .css
            .iter()
            .map(|href| {
                format!(
                    "<link rel=\"stylesheet\" href=\"{}\" media=\"{}\"{track} />",
                    escape_attr(href),
                    escape_attr(media)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `ERB::Util.html_escape`: escapes `& < > " '` and nothing else.
#[must_use]
pub fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_pack_tags_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        std::fs::write(
            &path,
            r#"{"entrypoints":{"application":{"assets":{
                "js":["/packs/js/runtime-1.js","/packs/js/application-2.js"],
                "css":["/packs/css/application-3.css"]}}}}"#,
        )
        .unwrap();
        let assets = Assets::new(path, false);

        assert_eq!(
            assets.javascript_tags("application", true),
            "<script src=\"/packs/js/runtime-1.js\" defer=\"defer\"></script>\n<script src=\"/packs/js/application-2.js\" defer=\"defer\"></script>"
        );
        assert_eq!(
            assets.stylesheet_tags("application", "all", Some("dynamic")),
            "<link rel=\"stylesheet\" href=\"/packs/css/application-3.css\" media=\"all\" data-turbo-track=\"dynamic\" />"
        );
        assert_eq!(assets.javascript_tags("missing", true), "");
    }
}
