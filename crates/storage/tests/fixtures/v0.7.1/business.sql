-- Representative v0.7.1 rows; only columns from immutable migrations 0001-0004.
INSERT INTO rule_versions (id,kind,version_tag,description,payload_sha256,status)
VALUES (100,'config','legacy','compatibility fixture','legacy-sha','superseded');
INSERT INTO feed_sources (id,category_key,source_key,display_name,feed_url,feed_kind,config_version)
VALUES (100,'ai','legacy','Legacy feed','https://example.test/feed','rss',100);
INSERT INTO feed_entries (id,source_id,feed_entry_uid,normalized_link,link_hash,title_raw,discovered_at,state,dedup_decision)
VALUES (100,100,'legacy','https://example.test/article','legacy-link','Legacy title','2026-07-18T00:00:00Z','persisted','fresh');
INSERT INTO articles (id,content_hash,canonical_link,title,body_text,extractor_strategy,extractor_version,content_quality,origin_feed_entry_id,state)
VALUES (100,'legacy-content','https://example.test/article','Legacy title','Legacy body','summary_fallback',100,'fallback',100,'ready_for_publish');
UPDATE feed_entries SET article_id=100 WHERE id=100;
INSERT INTO article_ai_results (id,article_id,prompt_version,output_schema_version,model_id,state,summary,tags_json,importance_score,keep_decision)
VALUES (100,100,100,100,'legacy-model','succeeded','Legacy summary','["legacy"]',85,1);
INSERT INTO publish_records (id,idempotency_key,category_key,report_date,target_timezone,render_version,selection_policy_version,state,snapshot_frozen_at)
VALUES (100,'legacy-report','ai','2026-07-18','UTC',100,100,'snapshot_frozen','2026-07-18T00:00:00Z');
INSERT INTO publish_items (id,publish_record_id,position,article_id,article_ai_result_id,frozen_title,frozen_summary,frozen_tags_json,frozen_score,frozen_canonical_link,frozen_source_display_name)
VALUES (100,100,1,100,100,'Frozen legacy title','Frozen legacy summary','["legacy"]',85,'https://example.test/article','Legacy feed');
