//! SVG icons from `app/views/icons/_<name>.html.erb`, rendered like the Rails `svg_icon`
//! helper. Each partial only substitutes `<%= local_assigns[:class] %>` (and, for one icon,
//! `<%= number %>`), so the files are used as they are.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use crate::assets::escape_attr;

pub struct Icons {
    dir: PathBuf,
    reload: bool,
    cache: RwLock<HashMap<String, Arc<str>>>,
}

impl Icons {
    #[must_use]
    pub fn new(dir: PathBuf, reload: bool) -> Self {
        Self {
            dir,
            reload,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Returns the icon markup, or `None` if there is no such icon.
    #[must_use]
    pub fn render(&self, name: &str, class: &str, locals: &[(&str, String)]) -> Option<String> {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }

        let source = self.source(name)?;
        let mut svg = source.replace("<%= local_assigns[:class] %>", &escape_attr(class));
        for (key, value) in locals {
            svg = svg.replace(&format!("<%= {key} %>"), &escape_attr(value));
        }
        Some(svg.trim_end().to_owned())
    }

    fn source(&self, name: &str) -> Option<Arc<str>> {
        if !self.reload
            && let Some(svg) = self.cache.read().expect("lock").get(name)
        {
            return Some(svg.clone());
        }

        let svg: Arc<str> = std::fs::read_to_string(self.dir.join(format!("_{name}.html.erb")))
            .ok()?
            .into();
        self.cache
            .write()
            .expect("lock")
            .insert(name.to_owned(), svg.clone());
        Some(svg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_rails_icon_partials() {
        let icons = Icons::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../app/views/icons"),
            false,
        );

        let svg = icons.render("info_circle", "w-6 \"h-6\"", &[]).unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("class=\"w-6 &quot;h-6&quot;\""));
        assert!(!svg.contains("<%"));

        assert!(icons.render("../secrets", "", &[]).is_none());
        assert!(icons.render("does_not_exist", "", &[]).is_none());
    }
}
