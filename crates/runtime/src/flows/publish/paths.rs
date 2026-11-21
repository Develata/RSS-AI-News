//! Pre-publish cross-category path collision check (real report date).
//!
//! `validate-config` renders every category's effective `path_template` with a
//! single sample date (ADR 0008). Templates such as `{YYYY}/{MM}/{DD}.md` and
//! `{YYYY}/{DD}/{MM}.md` pass that check yet render the same path whenever
//! month == day. Before the first write of a multi-category run, the real
//! paths for the run's report date are rendered with the publish renderer and
//! compared after normalization.

use rss_ai_news_config::validate::report_path_collisions;
use rss_ai_news_report::render_relative_path;

use crate::error::RuntimeError;

/// One category's effective `path_template`.
#[derive(Debug, Clone, Copy)]
pub struct ReportPathSpec<'a> {
    pub category_key: &'a str,
    pub path_template: &'a str,
}

/// Fails when two categories' report paths for `report_date` cannot both be
/// written, by the rule validate-config uses
/// ([`rss_ai_news_config::validate::report_path_collisions`]): equal after
/// `\` → `/`, dropping empty and `.` segments and folding letter case (one
/// file on a case-insensitive volume), or one path a whole-segment parent of
/// another (file vs directory).
pub fn check_report_path_collisions(
    specs: &[ReportPathSpec<'_>],
    report_date: &str,
) -> Result<(), RuntimeError> {
    let rendered = specs
        .iter()
        .map(|spec| {
            render_relative_path(spec.path_template, spec.category_key, report_date)
                .map(|path| (spec.category_key.to_string(), path))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let collisions = report_path_collisions(&rendered)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if collisions.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::Config(format!(
            "report paths collide for {report_date}: {}; give each category a distinct \
             path_template (e.g. include {{category_key}})",
            collisions.join("; ")
        )))
    }
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

    /// R1-P-B5: on a case-insensitive volume (macOS / Windows defaults) these
    /// are one file, so two "successful" reports would leave one behind.
    #[test]
    fn paths_differing_only_in_case_collide() {
        let specs = [
            ReportPathSpec {
                category_key: "ai",
                path_template: "News/{date}.md",
            },
            ReportPathSpec {
                category_key: "math",
                path_template: "news/{date}.md",
            },
        ];
        let error = check_report_path_collisions(&specs, "2026-01-03").expect_err("same file");
        assert!(error.to_string().contains("ai"), "{error}");
    }

    #[test]
    fn a_report_path_that_is_another_reports_directory_collides() {
        let specs = [
            ReportPathSpec {
                category_key: "ai",
                path_template: "news/{date}",
            },
            ReportPathSpec {
                category_key: "math",
                path_template: "news/{date}/{category_key}.md",
            },
        ];
        let error =
            check_report_path_collisions(&specs, "2026-01-03").expect_err("file vs directory");
        assert!(error.to_string().contains("parent directory"), "{error}");
    }

    #[test]
    fn invalid_report_date_is_an_error_not_a_pass() {
        assert!(check_report_path_collisions(&[MONTH_DAY], "2026-13-01").is_err());
    }
}
