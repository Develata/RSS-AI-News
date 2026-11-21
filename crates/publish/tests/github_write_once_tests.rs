//! Raw-socket wire tests for what reaches GitHub during one publish attempt.
//!
//! R3F-B1: the publish client sends every request once. A transparent
//! client-side resend of a write that already landed (connection dropped
//! after the server applied it) would hit "file exists, sha missing" (422) or
//! a moved ref and hide the landed write from the HEAD verification; a resent
//! 429 would spend the rate-limit window inside one claim.

use std::sync::{Arc, Mutex};

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, PublishError, PublishTarget};

/// `(request line, request body)` of every request received, in order.
type Log = Arc<Mutex<Vec<(String, String)>>>;

/// What the server does with one request: answer `(status line, extra
/// headers, JSON body)`, or `None` = read the request, then drop the
/// connection without any response (the write was applied, the answer lost).
type Reply = Option<(&'static str, String, String)>;

/// Serves connections until the test ends. `route` sees the current request
/// line and body plus the log of every request so far (current included).
async fn router<F>(route: F) -> (String, Log)
where
    F: Fn(&str, &str, &[(String, String)]) -> Reply + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::default();
    let server_log = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head).into_owned();
            let line = text.lines().next().unwrap_or_default().to_string();
            let length = text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let mut body = vec![0_u8; length];
            if socket.read_exact(&mut body).await.is_err() {
                continue;
            }
            let body = String::from_utf8_lossy(&body).into_owned();
            let reply = {
                let mut log = server_log.lock().unwrap();
                log.push((line.clone(), body.clone()));
                route(&line, &body, &log)
            };
            let Some((status, headers, reply)) = reply else {
                drop(socket);
                continue;
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (uri, log)
}

fn target(uri: &str) -> GitHubTarget {
    GitHubTarget::with_base_uri(
        GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from("write-once-test-token"),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "main".to_string(),
            path_prefix: "reports".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        uri,
    )
    .unwrap()
}

fn report() -> RenderedReport {
    RenderedReport {
        publish_record_id: 1,
        category_key: "tech".to_string(),
        report_date: "2026-11-22".to_string(),
        markdown_content: "# tech\n".to_string(),
        relative_path: "tech/2026-11-22.md".to_string(),
    }
}

/// `printf '# tech\n' | git hash-object --stdin`
const TECH_BLOB: &str = "06e96727937de1d434851eb8006ae650b3101c29";
const CONTENTS: &str = "/repos/owner/repo/contents/reports/tech/2026-11-22.md";

fn count(log: &[(String, String)], prefix: &str) -> usize {
    log.iter()
        .filter(|(line, _)| line.starts_with(prefix))
        .count()
}

fn json(status: &'static str, body: &str) -> Reply {
    Some((status, String::new(), body.to_string()))
}

/// R3F-B1: the create PUT lands and the connection drops. A resend would be
/// refused with 422 (the file now exists and the PUT carries no `sha`); the
/// single PUT is instead followed by the read-only HEAD check, which finds
/// the landed commit and records it.
#[tokio::test]
async fn landed_put_with_a_dropped_connection_is_sent_once_and_verified() {
    let (uri, log) = router(|line, _body, log| {
        let puts = count(log, "PUT ");
        if line.starts_with(&format!("GET {CONTENTS}?ref=main ")) {
            if puts == 0 {
                json("404 Not Found", r#"{"message":"Not Found"}"#)
            } else {
                json("200 OK", &format!(r#"{{"sha":"{TECH_BLOB}"}}"#))
            }
        } else if line.starts_with(&format!("PUT {CONTENTS} ")) {
            if puts == 1 {
                None
            } else {
                json(
                    "422 Unprocessable Entity",
                    r#"{"message":"Invalid request.\n\n\"sha\" wasn't supplied."}"#,
                )
            }
        } else if line.starts_with("GET /repos/owner/repo/git/ref/heads/main ") {
            json("200 OK", r#"{"object":{"sha":"landed-commit"}}"#)
        } else if line.starts_with(&format!("GET {CONTENTS}?ref=landed-commit ")) {
            json("200 OK", &format!(r#"{{"sha":"{TECH_BLOB}"}}"#))
        } else {
            json("500 Internal Server Error", r#"{"message":"unexpected"}"#)
        }
    })
    .await;

    let artifact = target(&uri)
        .publish(&report())
        .await
        .expect("the landed PUT is verified against HEAD");

    assert_eq!(artifact.commit_sha.as_deref(), Some("landed-commit"));
    let log = log.lock().unwrap().clone();
    assert_eq!(count(&log, "PUT "), 1, "exactly one write request: {log:?}");
}

/// R3F-B1: a 429 is returned to the publish layer at once (one request), so
/// the stage can persist GitHub's deadline instead of the client resending
/// inside the window.
#[tokio::test]
async fn rate_limited_request_is_not_resent_by_the_client() {
    let (uri, log) = router(|_line, _body, _log| {
        Some((
            "429 Too Many Requests",
            "retry-after: 120\r\n".to_string(),
            r#"{"message":"API rate limit exceeded"}"#.to_string(),
        ))
    })
    .await;

    let error = target(&uri).publish(&report()).await.unwrap_err();

    assert!(
        matches!(error, PublishError::GitHubRateLimit { .. }),
        "{error:?}"
    );
    assert_eq!(log.lock().unwrap().len(), 1, "one request in the window");
}
