use std::path::{Path, PathBuf};

use loco_views::{Views, ViewsConfig};
use serde_json::json;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Renders a page through the ported plain layout with the real locale file and icons.
#[test]
fn renders_page_in_plain_layout() {
    let dir = tempfile::tempdir().unwrap();
    let templates = dir.path().join("templates");
    copy_dir(&repo_root().join("rust/templates"), &templates);
    std::fs::write(
        templates.join("page.html"),
        r#"{% extends "layouts/plain.html" %}{% block content %}<p>{{ t('hello_name', name=name) }}</p>{% endblock %}"#,
    )
    .unwrap();
    let manifest = dir.path().join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{"entrypoints":{"application":{"assets":{"js":["/packs/js/application.js"],"css":["/packs/css/application.css"]}}}}"#,
    )
    .unwrap();

    let views = Views::new(&ViewsConfig {
        templates_dir: templates,
        manifest_file: manifest,
        ..ViewsConfig::from_repo_root(&repo_root(), false)
    })
    .unwrap();

    let html = views
        .render(
            "page.html",
            json!({
                "locale": "de-DE",
                "csrf_token": "tok\"en",
                "name": "<script>",
                "root_url": "https://contracts.example/",
                "flash": { "notice": "Gespeichert" },
            }),
        )
        .unwrap();

    assert!(html.contains("<html data-theme=\"docuseal\" lang=\"de-DE\">"));
    assert!(html.contains(&format!(
        "<p>{}</p>",
        views.t("de-DE", "hello_name", &[("name", "&lt;script&gt;".into())])
    )));
    assert!(html.contains("<meta name=\"csrf-token\" content=\"tok&quot;en\" />"));
    assert!(html.contains("<script src=\"/packs/js/application.js\" defer=\"defer\"></script>"));
    assert!(html.contains("data-turbo-track=\"dynamic\""));
    assert!(
        html.contains("content=\"https://contracts.example/preview.png\""),
        "{html}"
    );
    assert!(html.contains("<span>Gespeichert</span>"));
    assert!(html.contains("stroke-info flex-none w-6 h-6"));
    assert!(!html.contains("<script>"));
}
