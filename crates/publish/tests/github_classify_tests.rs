use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::PublishError;
use rss_ai_news_publish::github::classify::classify_github_status;
use time::OffsetDateTime;

#[test]
fn status_401_maps_to_auth_failed() {
    let error = classify_github_status(401, "bad credentials".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubAuthFailed { message: reason, .. } if reason == "bad credentials"
    ));
    assert!(!error.is_retryable());
}

#[test]
fn status_429_maps_to_rate_limit_with_reset_header() {
    let error = classify_github_status(429, "rate limited".to_string(), Some(1_800));

    assert!(matches!(
        error,
        PublishError::GitHubRateLimit { reset_at, .. }
            if reset_at == OffsetDateTime::from_unix_timestamp(1_800).unwrap()
    ));
    assert!(error.is_retryable());
}

#[test]
fn status_500_maps_to_retryable_api_error() {
    let error = classify_github_status(500, "server error".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubApiError { status: 500, message, .. } if message == "server error"
    ));
    assert!(error.is_retryable());
}

#[test]
fn status_409_maps_to_retryable_api_error() {
    let error = classify_github_status(409, "reference update conflict".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubApiError { status: 409, message, .. }
            if message == "reference update conflict"
    ));
    assert!(error.is_retryable());
}

#[test]
fn status_422_maps_to_permanent_api_error() {
    let error = classify_github_status(422, "validation failed".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubApiError { status: 422, message, .. } if message == "validation failed"
    ));
    assert!(!error.is_retryable());
}

#[test]
fn status_403_naming_a_rate_limit_maps_to_rate_limit() {
    for message in [
        "API rate limit exceeded for user ID 1.",
        "You have exceeded a secondary rate limit. Please wait a few minutes.",
    ] {
        let error = classify_github_status(403, message.to_string(), Some(1_800));

        assert!(
            matches!(
                error,
                PublishError::GitHubRateLimit { reset_at, .. }
                    if reset_at == OffsetDateTime::from_unix_timestamp(1_800).unwrap()
            ),
            "{message}: {error:?}"
        );
        assert!(error.is_retryable());
    }
}

#[test]
fn status_403_without_rate_limit_signal_maps_to_auth_failed() {
    let error = classify_github_status(403, "Must have admin rights".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubAuthFailed { message: reason, .. } if reason == "Must have admin rights"
    ));
    assert!(!error.is_retryable());
}

// === R1-P-S11: Retry-After in both RFC 9110 forms, checked arithmetic ===

use rss_ai_news_publish::github::classify::{
    MAX_RETRY_AFTER_SECONDS, RateLimitHints, RetryAfter, classify_github_response,
};

fn hints_with_retry_after(value: &str) -> RateLimitHints {
    let mut headers = http::HeaderMap::new();
    headers.insert("retry-after", value.parse().unwrap());
    RateLimitHints::from_headers(&headers)
}

#[test]
fn forbidden_with_an_http_date_retry_after_is_a_rate_limit_until_that_date() {
    let hints = hints_with_retry_after("Sun, 22 Nov 2026 08:30:00 GMT");
    let expected = OffsetDateTime::from_unix_timestamp(1_795_336_200).unwrap();
    assert_eq!(hints.retry_after, Some(RetryAfter::At(expected)));

    let error = classify_github_response(403, "Forbidden".to_string(), &hints);

    assert!(
        matches!(error, PublishError::GitHubRateLimit { reset_at, .. } if reset_at >= expected),
        "{error:?}"
    );
    assert!(error.is_retryable());
}

/// R3F-S11: RFC 9110 §5.6.7 recipients also accept the obsolete RFC 850
/// and asctime forms; each yields the real deadline, not the 60 s fallback.
#[test]
fn obsolete_http_date_forms_of_retry_after_give_the_real_deadline() {
    let expected = OffsetDateTime::from_unix_timestamp(1_795_336_200).unwrap();
    for value in ["Sunday, 22-Nov-26 08:30:00 GMT", "Sun Nov 22 08:30:00 2026"] {
        let hints = hints_with_retry_after(value);
        assert_eq!(hints.retry_after, Some(RetryAfter::At(expected)), "{value}");
        let now = expected - time::Duration::hours(2);
        assert_eq!(hints.reset_at_from(now), expected, "{value}");
    }
}

#[test]
fn unparsable_retry_after_still_marks_a_403_as_rate_limited() {
    let hints = hints_with_retry_after("soon");
    assert_eq!(hints.retry_after, Some(RetryAfter::Unparsable));

    let error = classify_github_response(403, "Forbidden".to_string(), &hints);

    assert!(
        matches!(error, PublishError::GitHubRateLimit { .. }),
        "{error:?}"
    );
}

#[test]
fn oversized_retry_after_seconds_are_capped_without_overflow() {
    let now = OffsetDateTime::now_utc();
    for value in ["9223372036854775807", "99999999999999999999999"] {
        let reset_at = hints_with_retry_after(value).reset_at_from(now);
        assert_eq!(
            reset_at,
            now + time::Duration::seconds(MAX_RETRY_AFTER_SECONDS as i64),
            "{value}"
        );
    }
}

#[test]
fn past_http_date_retry_after_means_now() {
    let now = OffsetDateTime::now_utc();
    let reset_at = hints_with_retry_after("Sun, 06 Nov 1994 08:49:37 GMT").reset_at_from(now);
    assert_eq!(reset_at, now);
}

// === R3F-S12: the latest applicable deadline wins ===

fn hints(retry_after: Option<&str>, remaining: Option<i64>, reset: Option<i64>) -> RateLimitHints {
    let mut headers = http::HeaderMap::new();
    if let Some(value) = retry_after {
        headers.insert("retry-after", value.parse().unwrap());
    }
    if let Some(value) = remaining {
        headers.insert("x-ratelimit-remaining", value.to_string().parse().unwrap());
    }
    if let Some(value) = reset {
        headers.insert("x-ratelimit-reset", value.to_string().parse().unwrap());
    }
    RateLimitHints::from_headers(&headers)
}

/// Exhausted quota (`remaining: 0`) binds `x-ratelimit-reset` even when a
/// shorter `retry-after` is present: waiting only for the latter would retry
/// into a still-exhausted quota.
#[test]
fn exhausted_quota_reset_later_than_retry_after_wins() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let reset = 1_800_000_000 + 3_000;
    let at = hints(Some("60"), Some(0), Some(reset)).reset_at_from(now);
    assert_eq!(at, OffsetDateTime::from_unix_timestamp(reset).unwrap());
}

/// A `retry-after` later than the reset wins too (the latest applies).
#[test]
fn retry_after_later_than_an_exhausted_quota_reset_wins() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let at = hints(Some("600"), Some(0), Some(1_800_000_000 + 60)).reset_at_from(now);
    assert_eq!(at, now + time::Duration::seconds(600));
}

/// With quota left, `x-ratelimit-reset` is not a wait GitHub asked for: a
/// secondary limit's `retry-after` alone decides.
#[test]
fn reset_does_not_apply_while_quota_remains_and_retry_after_is_given() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let at = hints(Some("120"), Some(42), Some(1_800_000_000 + 3_000)).reset_at_from(now);
    assert_eq!(at, now + time::Duration::seconds(120));
}

#[test]
fn status_401_stays_auth_failed_even_when_quota_is_exhausted() {
    let error = rss_ai_news_publish::github::classify::classify_github_response(
        401,
        "Bad credentials".to_string(),
        &rss_ai_news_publish::github::classify::RateLimitHints {
            remaining: Some(0),
            ..Default::default()
        },
    );

    assert!(matches!(error, PublishError::GitHubAuthFailed { .. }));
}
