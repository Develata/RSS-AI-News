use std::time::Duration;

use rss_ai_news_ai::{AiClientConfig, AiTask, OpenAiCompatClient};

pub fn test_client(api_base: String) -> OpenAiCompatClient {
    OpenAiCompatClient::new(AiClientConfig {
        api_base,
        api_key: "sk-test".to_string(),
        request_timeout: Duration::from_secs(2),
    })
    .expect("test client config must be valid")
}

pub fn test_client_with_timeout(api_base: String, request_timeout: Duration) -> OpenAiCompatClient {
    OpenAiCompatClient::new(AiClientConfig {
        api_base,
        api_key: "sk-test".to_string(),
        request_timeout,
    })
    .expect("test client config must be valid")
}

pub fn test_task() -> AiTask {
    AiTask {
        article_ai_result_id: 7,
        article_id: 11,
        title: "测试标题".to_string(),
        body_text: "测试正文".to_string(),
        category_key: "ai".to_string(),
        prompt_template: "标题：{title}\n分类：{category_key}\n正文：{body_text}".to_string(),
        model_id: "gpt-test".to_string(),
        max_tokens: 256,
        temperature: 0.2,
    }
}
