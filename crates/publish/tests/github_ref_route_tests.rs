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

/// R1-P-B2 transport path: the connection is closed after the PATCH request
/// was read (no response at all — the ref update landed, the answer was
/// lost). The batch then reads HEAD back and records the landed commit
/// instead of failing. Responses are chosen by request line, so a client-side
/// resend of the PATCH is answered the same way.
#[tokio::test]
async fn transport_failure_after_the_ref_patch_is_verified_against_head() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut ref_reads = 0;
        let mut patches = 0;
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
            let _ = socket.read_exact(&mut body).await;
            let (status, reply) = if line.starts_with("PATCH ") {
                patches += 1;
                // Applied, but the answer never arrives.
                drop(socket);
                continue;
            } else if line.starts_with("GET /repos/owner/repo/git/ref/heads/main ") {
                ref_reads += 1;
                let sha = if ref_reads == 1 { "head" } else { "new-commit" };
                ("200 OK", format!(r#"{{"object":{{"sha":"{sha}"}}}}"#))
            } else if line.starts_with("GET /repos/owner/repo/git/commits/head ") {
                ("200 OK", r#"{"tree":{"sha":"tree"}}"#.to_string())
            } else if line.contains("?ref=head ") {
                ("404 Not Found", r#"{"message":"Not Found"}"#.to_string())
            } else if line.contains("/tech/2026-11-22.md?ref=new-commit ") {
                (
                    "200 OK",
                    r#"{"sha":"06e96727937de1d434851eb8006ae650b3101c29"}"#.to_string(),
                )
            } else if line.contains("/ai/2026-11-22.md?ref=new-commit ") {
                (
                    "200 OK",
                    r#"{"sha":"ac4465c28b0c1b4f8a63e6a696f955e5b8a696d5"}"#.to_string(),
                )
            } else if line.starts_with("POST /repos/owner/repo/git/trees ") {
                ("201 Created", r#"{"sha":"new-tree"}"#.to_string())
            } else if line.starts_with("POST /repos/owner/repo/git/commits ") {
                ("201 Created", r#"{"sha":"new-commit"}"#.to_string())
            } else {
                (
                    "500 Internal Server Error",
                    r#"{"message":"unexpected"}"#.to_string(),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            if ref_reads >= 2 && line.contains("/ai/2026-11-22.md?ref=new-commit ") {
                break;
            }
        }
        patches
    });

    let batch = target(&uri, "main")
        .publish_many(&reports())
        .await
        .expect("the lost ref update is verified against HEAD");

    assert_eq!(batch.commit_sha.as_deref(), Some("new-commit"));
    assert!(server.await.unwrap() >= 1, "the PATCH was sent");
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
