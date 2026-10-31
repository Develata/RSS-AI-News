//! Raw passthrough of the optional typed-decision sections (ADR 0010): the
//! `[decision]` table of app.toml and each category's `[category.decision]`.
//! This crate does not interpret them; `rss-ai-news-decision` does, so the
//! feature can be removed without touching the typed config model.

use std::fmt;

use serde::Deserialize;

/// Debug prints where sections are, never their raw values: `LoadedConfig`
/// derives Debug, and a raw table could hold a URL with credentials.
#[derive(Clone, Default)]
pub struct DecisionSections {
    pub app: Option<toml::Value>,
    pub categories: Vec<CategoryDecisionSection>,
}

#[derive(Clone)]
pub struct CategoryDecisionSection {
    /// `categories/<file>`, the same source label config diagnostics use.
    pub source: String,
    pub category_key: String,
    pub value: toml::Value,
}

impl fmt::Debug for DecisionSections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionSections")
            .field("app_present", &self.app.is_some())
            .field("categories", &self.categories)
            .finish()
    }
}

impl fmt::Debug for CategoryDecisionSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CategoryDecisionSection")
            .field("source", &self.source)
            .field("category_key", &self.category_key)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct AppPart {
    #[serde(default)]
    decision: Option<toml::Value>,
}

#[derive(Deserialize)]
struct CategoryPart {
    category: CategoryInner,
}

#[derive(Deserialize)]
struct CategoryInner {
    key: String,
    #[serde(default)]
    decision: Option<toml::Value>,
}

/// Re-read the already parsed files for the two raw sections.
pub(crate) fn extract(
    app_content: &str,
    category_contents: &[(String, String)],
) -> Result<DecisionSections, (String, toml::de::Error)> {
    let app = toml::from_str::<AppPart>(app_content)
        .map_err(|error| ("app.toml".to_string(), error))?
        .decision;
    let mut categories = Vec::new();
    for (file, content) in category_contents {
        let source = format!("categories/{file}");
        let part =
            toml::from_str::<CategoryPart>(content).map_err(|error| (source.clone(), error))?;
        if let Some(value) = part.category.decision {
            categories.push(CategoryDecisionSection {
                source,
                category_key: part.category.key,
                value,
            });
        }
    }
    Ok(DecisionSections { app, categories })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_sections_stay_absent() {
        let sections = extract(
            "schema_version = \"1\"",
            &[(
                "ai.toml".to_string(),
                "[category]\nkey = \"ai\"\ndisplay_name = \"AI\"\npriority = 1".to_string(),
            )],
        )
        .unwrap();
        assert!(sections.app.is_none());
        assert!(sections.categories.is_empty());
    }

    #[test]
    fn present_sections_pass_through_untouched() {
        let category = r#"
[category]
key = "ai"
display_name = "AI"
priority = 1

[[category.decision.questions]]
id = "q_a"
type = "noul"
instructions = "x"
"#;
        let sections = extract(
            "[decision]\nprovider = \"laya\"\nanything = 1",
            &[("ai.toml".to_string(), category.to_string())],
        )
        .unwrap();
        assert_eq!(
            sections.app.unwrap().get("anything").unwrap().as_integer(),
            Some(1)
        );
        let category = &sections.categories[0];
        assert_eq!(category.source, "categories/ai.toml");
        assert_eq!(category.category_key, "ai");
        assert!(category.value.get("questions").unwrap().is_array());
    }

    #[test]
    fn debug_never_prints_raw_values() {
        let sections = extract(
            "[decision]\nprovider = \"laya\"\nbase_url = \"http://u:sk-secret@host\"",
            &[],
        )
        .unwrap();
        let debug = format!("{sections:?}");
        assert!(debug.contains("app_present: true"));
        assert!(!debug.contains("sk-secret"));
    }
}
