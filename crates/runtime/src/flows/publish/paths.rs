//! Pre-publish cross-category path collision check (real report date).
//!
//! `validate-config` renders every category's effective `path_template` with a
//! single sample date (ADR 0008). Templates such as `{YYYY}/{MM}/{DD}.md` and
//! `{YYYY}/{DD}/{MM}.md` pass that check yet render the same path whenever
//! month == day. Before the first write of a multi-category run, the real
//! paths for the run's report date are rendered with the publish renderer and
//! compared after normalization.

use std::collections::BTreeMap;

use rss_ai_news_report::render_relative_path;

use crate::error::RuntimeError;

/// One category's effective `path_template`.
#[derive(Debug, Clone, Copy)]
pub struct ReportPathSpec<'a> {
    pub category_key: &'a str,
    pub path_template: &'a str,
}

/// Fails when two categories would write the same report path for
/// `report_date`. Paths are compared after `\` → `/` and dropping empty and
/// `.` segments (`a//./b.md` ≡ `a/b.md`); case is significant, as on the
/// publish targets' own path checks.
pub fn check_report_path_collisions(
    specs: &[ReportPathSpec<'_>],
    report_date: &str,
) -> Result<(), RuntimeError> {
    let mut by_path: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for spec in specs {
        let rendered = render_relative_path(spec.path_template, spec.category_key, report_date)?;
        by_path
            .entry(normalize(&rendered))
            .or_default()
            .push(spec.category_key);
    }
    let collisions = by_path
        .into_iter()
        .filter(|(_, keys)| keys.len() > 1)
        .map(|(path, mut keys)| {
            keys.sort_unstable();
            format!("{path:?} <- {keys:?}")
        })
        .collect::<Vec<_>>();
    if collisions.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::Config(format!(
            "report paths collide for {report_date}: {}; give each category a distinct \
             path_template (e.g. include {{category_key}})",
            collisions.join(", ")
        )))
    }
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONTH_DAY: ReportPathSpec<'static> = ReportPathSpec {
        category_key: "ai",
        path_template: "{YYYY}/{MM}/{DD}.md",
    };
    const DAY_MONTH: ReportPathSpec<'static> = ReportPathSpec {
        category_key: "math",
        path_template: "{YYYY}/{DD}/{MM}.md",
    };

    #[test]
    fn month_day_and_day_month_templates_collide_on_eleven_eleven() {
        let error = check_report_path_collisions(&[MONTH_DAY, DAY_MONTH], "2026-11-11")
            .expect_err("both render 2026/11/11.md");
        let message = error.to_string();
        assert!(message.contains("2026/11/11.md"), "{message}");
        assert!(
            message.contains("ai") && message.contains("math"),
            "{message}"
        );
    }

    #[test]
    fn month_day_and_day_month_templates_do_not_collide_on_other_dates() {
        check_report_path_collisions(&[MONTH_DAY, DAY_MONTH], "2026-11-12").expect("distinct");
    }

    #[test]
    fn collision_is_detected_after_path_normalization() {
        let specs = [
            ReportPathSpec {
                category_key: "ai",
                path_template: "news//{date}.md",
            },
            ReportPathSpec {
                category_key: "math",
                path_template: "./news/{date}.md",
            },
        ];
        assert!(check_report_path_collisions(&specs, "2026-01-03").is_err());
    }

    #[test]
    fn invalid_report_date_is_an_error_not_a_pass() {
        assert!(check_report_path_collisions(&[MONTH_DAY], "2026-13-01").is_err());
    }
}
