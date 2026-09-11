-- Bound foreign-key lookup work when expiring raw artifact payloads.
CREATE INDEX idx_articles_body_html_artifact_id
    ON articles (body_html_artifact_id);
CREATE INDEX idx_article_ai_results_raw_response_artifact_id
    ON article_ai_results (raw_response_artifact_id);
