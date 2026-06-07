-- 回滚 W14-A P3：移除 effective_model_id（PostgreSQL DROP COLUMN；该列无索引/视图依赖）。
ALTER TABLE article_ai_results DROP COLUMN effective_model_id;
