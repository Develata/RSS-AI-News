use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::PublishError;
use rss_ai_news_publish::github::classify::classify_github_status;
use time::OffsetDateTime;

#[test]
fn status_401_maps_to_auth_failed() {
    let error = classify_github_status(401, "bad credentials".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubAuthFailed(reason) if reason == "bad credentials"
    ));
    assert!(!error.is_retryable());
}

#[test]
fn status_429_maps_to_rate_limit_with_reset_header() {
    let error = classify_github_status(429, "rate limited".to_string(), Some(1_800));

    assert!(matches!(
        error,
        PublishError::GitHubRateLimit { reset_at }
            if reset_at == OffsetDateTime::from_unix_timestamp(1_800).unwrap()
    ));
    assert!(error.is_retryable());
}

#[test]
fn status_500_maps_to_retryable_api_error() {
    let error = classify_github_status(500, "server error".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubApiError { status: 500, message } if message == "server error"
    ));
    assert!(error.is_retryable());
}

#[test]
fn status_409_maps_to_retryable_api_error() {
    let error = classify_github_status(409, "reference update conflict".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubApiError { status: 409, message }
            if message == "reference update conflict"
    ));
    assert!(error.is_retryable());
}

#[test]
fn status_422_maps_to_permanent_api_error() {
    let error = classify_github_status(422, "validation failed".to_string(), None);

    assert!(matches!(
        &error,
        PublishError::GitHubApiError { status: 422, message } if message == "validation failed"
    ));
    assert!(!error.is_retryable());
}
