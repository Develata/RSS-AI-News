-- W14-A P3：article_ai_results 增加 effective_model_id（fallback 实际成功调用的模型）。
--
-- model_id 是幂等键 ON CONFLICT(article_id, prompt_version, output_schema_version, model_id)，
-- 不可变、记"首选/锚定模型"；effective_model_id 为非键列，记实际生效模型，供按实际模型统计/审计。
--
-- 迁移策略（codex P3 评审）：新增 nullable 列 → 一次性回填既有行 = model_id
-- （SQLite/PG 都不能用 column default 引用同行 model_id）→ 之后成功 release 必写实际模型。
ALTER TABLE article_ai_results ADD COLUMN effective_model_id TEXT;

UPDATE article_ai_results
   SET effective_model_id = model_id
 WHERE effective_model_id IS NULL;
