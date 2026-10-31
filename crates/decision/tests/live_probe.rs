//! Manual live probe (ADR 0010 §7): one small request to a real provider to
//! check connectivity and the wire contract, and to observe the precision Jev
//! actually returns. Ignored by default; never part of CI or quality evidence.
//!
//! ```text
//! RSS_AI_NEWS_DECISION_PROBE_PROVIDER=jev \
//!   cargo test -p rss-ai-news-decision --test live_probe -- --ignored --nocapture
//! ```
//!
//! Optional: `..._BASE_URL` (Jev default https://api.typesafe.ai), `..._MODEL`
//! (jev-1.13.0 / multilingual), `..._KEY_ENV` (JEV_API_KEY / LAYA_API_KEY).
//! The key is read from that process variable, or else from `..._ENV_FILE`
//! (a dotenv file, default `.env` in the workspace root). It is never printed.

use std::{env, fs, path::Path, time::Duration};

use rss_ai_news_decision::{
    ChoiceOption, ClientSettings, DecisionAnswer, DecisionClient, DecisionRequest, Provider,
    QuestionKind, QuestionSpec, SystemOneClient,
};
use rss_ai_news_domain::SecretString;
use url::Url;

fn var(name: &str) -> Option<String> {
    env::var(format!("RSS_AI_NEWS_DECISION_PROBE_{name}"))
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn key_from_env_file(path: &Path, name: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == name)
            .then(|| {
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string()
            })
            .filter(|value| !value.is_empty())
    })
}

/// Digits after the decimal point in Rust's shortest round-trip form.
fn decimals(value: f64) -> usize {
    let text = value.to_string();
    text.split_once('.')
        .map_or(0, |(_, fraction)| fraction.len())
}

#[tokio::test]
#[ignore = "manual: needs a reachable provider and an API key"]
async fn live_probe() {
    let provider = match var("PROVIDER").as_deref() {
        Some("jev") => Provider::Jev,
        Some("laya") => Provider::Laya,
        other => panic!("set RSS_AI_NEWS_DECISION_PROBE_PROVIDER to jev or laya, got {other:?}"),
    };
    let (default_url, default_model, default_key) = match provider {
        Provider::Jev => ("https://api.typesafe.ai", "jev-1.13.0", "JEV_API_KEY"),
        Provider::Laya => ("", "multilingual", "LAYA_API_KEY"),
    };
    let base_url = var("BASE_URL").unwrap_or_else(|| default_url.to_string());
    assert!(
        !base_url.is_empty(),
        "set RSS_AI_NEWS_DECISION_PROBE_BASE_URL"
    );
    let key_env = var("KEY_ENV").unwrap_or_else(|| default_key.to_string());
    let env_file = var("ENV_FILE")
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env").to_string());
    let key = env::var(&key_env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| key_from_env_file(Path::new(&env_file), &key_env))
        .unwrap_or_else(|| panic!("{key_env} is not set in the environment or {env_file}"));

    let client = SystemOneClient::new(
        ClientSettings {
            provider,
            base_url: Url::parse(&base_url).expect("base url"),
            model: var("MODEL").unwrap_or_else(|| default_model.to_string()),
            request_timeout: Duration::from_secs(30),
            max_state_chars: 2000,
            laya_max_len: 2048,
            use_env_proxy: true,
        },
        SecretString::from(key),
    )
    .expect("client");
    let questions = [
        QuestionSpec {
            id: "q_marketing".to_string(),
            instructions: "Is this article mainly vendor marketing or PR?".to_string(),
            kind: QuestionKind::Noul {
                yes: None,
                no: None,
            },
        },
        QuestionSpec {
            id: "q_kind".to_string(),
            instructions: "What kind of article is this?".to_string(),
            kind: QuestionKind::Choice {
                options: vec![
                    ChoiceOption {
                        label: "release".to_string(),
                        description: Some("a new model or software release".to_string()),
                    },
                    ChoiceOption {
                        label: "paper".to_string(),
                        description: Some("a research paper".to_string()),
                    },
                    ChoiceOption {
                        label: "other".to_string(),
                        description: None,
                    },
                ],
            },
        },
        QuestionSpec {
            id: "q_importance".to_string(),
            instructions: "How important is this news for AI practitioners?".to_string(),
            kind: QuestionKind::Score {
                levels: vec![
                    "trivial".to_string(),
                    "notable".to_string(),
                    "major".to_string(),
                ],
            },
        },
    ];
    let response = client
        .decide(&DecisionRequest {
            title: "Open-weight model release",
            body: "A lab released a 7B open-weight language model under Apache-2.0, \
                   with evaluation results on common reasoning benchmarks. \
                   某实验室以 Apache-2.0 许可发布了 7B 开源权重语言模型，并公布了推理基准评测结果。",
            questions: &questions,
        })
        .await
        .expect("live decision request");

    println!("execution: {:?}", response.execution);
    println!("usage: {:?}", response.usage);
    let mut max_decimals = 0;
    for answer in &response.answers {
        println!("{}: {:?}", answer.id, answer.answer);
        let probabilities: &[f64] = match &answer.answer {
            DecisionAnswer::Noul { p_yes, .. } => std::slice::from_ref(p_yes),
            DecisionAnswer::Choice { probabilities, .. }
            | DecisionAnswer::Score { probabilities, .. } => probabilities,
        };
        max_decimals = probabilities
            .iter()
            .map(|value| decimals(*value))
            .fold(max_decimals, usize::max);
    }
    println!("observed probability precision: up to {max_decimals} decimals");
}
