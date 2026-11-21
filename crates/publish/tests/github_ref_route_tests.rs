//! R1-P-B3: the Git Data ref routes (`GET git/ref/heads/{branch}`,
//! `PATCH git/refs/heads/{branch}`) carry the branch percent-encoded per
//! `/`-separated segment. Unencoded, `release#daily` is cut at `#` by the URI
//! parser and the batch reads or moves the branch `release` instead.
//!
//! The assertions read the raw request lines as they left the client.

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, PublishTarget};

/// Answers one connection per `(status line, JSON body)` and records each
/// request line exactly as received.
async fn raw_server(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut lines = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let head_text = String::from_utf8_lossy(&head).into_owned();
            lines.push(head_text.lines().next().unwrap_or_default().to_string());
            let length = head_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let mut request_body = vec![0_u8; length];
            socket.read_exact(&mut request_body).await.unwrap();
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        lines
    });
    (uri, handle)
}

fn target(uri: &str, branch: &str) -> GitHubTarget {
    GitHubTarget::with_base_uri(
        GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from("ref-route-test-token"),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: branch.to_string(),
            path_prefix: "reports".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        uri,
    )
    .unwrap()
}

fn reports() -> Vec<RenderedReport> {
    ["tech", "ai"]
        .into_iter()
        .enumerate()
        .map(|(index, category)| RenderedReport {
            publish_record_id: index as i64 + 1,
            category_key: category.to_string(),
            report_date: "2026-11-22".to_string(),
            markdown_content: format!("# {category}\n"),
            relative_path: format!("{category}/2026-11-22.md"),
        })
        .collect()
}

#[tokio::test]
async fn batch_ref_routes_encode_hash_percent_and_keep_slashes_in_the_branch() {
    let (uri, server) = raw_server(vec![
        ("200 OK", r#"{"object":{"sha":"head"}}"#),
        ("200 OK", r#"{"tree":{"sha":"tree"}}"#),
        ("404 Not Found", r#"{"message":"Not Found"}"#),
        ("201 Created", r#"{"sha":"new-tree"}"#),
        ("201 Created", r#"{"sha":"new-commit"}"#),
        ("200 OK", r#"{"object":{"sha":"new-commit"}}"#),
    ])
    .await;

    let batch = target(&uri, "release#daily/50%")
        .publish_many(&reports())
        .await
        .expect("batch publishes");

    assert_eq!(batch.commit_sha.as_deref(), Some("new-commit"));
    let lines = server.await.unwrap();
    assert_eq!(
        lines,
        vec![
            "GET /repos/owner/repo/git/ref/heads/release%23daily/50%25 HTTP/1.1".to_string(),
            "GET /repos/owner/repo/git/commits/head HTTP/1.1".to_string(),
            "GET /repos/owner/repo/contents/reports/tech/2026-11-22.md?ref=head HTTP/1.1"
                .to_string(),
            "POST /repos/owner/repo/git/trees HTTP/1.1".to_string(),
            "POST /repos/owner/repo/git/commits HTTP/1.1".to_string(),
            "PATCH /repos/owner/repo/git/refs/heads/release%23daily/50%25 HTTP/1.1".to_string(),
        ]
    );
    // The recorded receipt keeps the logical branch name.
    assert_eq!(
        batch.artifacts[0].remote_target.as_deref(),
        Some("github://owner/repo/release#daily/50%/reports/tech/2026-11-22.md")
    );
}

#[tokio::test]
async fn plain_hierarchical_branch_ref_route_is_unchanged() {
    let (uri, server) = raw_server(vec![
        ("200 OK", r#"{"object":{"sha":"head"}}"#),
        ("404 Not Found", r#"{"message":"Not Found"}"#),
    ])
    .await;

    let found = target(&uri, "release/2026")
        .find_published(&reports()[..1])
        .await
        .expect("lookup succeeds");

    assert!(found.is_none());
    let lines = server.await.unwrap();
    assert_eq!(
        lines[0],
        "GET /repos/owner/repo/git/ref/heads/release/2026 HTTP/1.1"
    );
}
