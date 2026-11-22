-- The remote write whose outcome is not on record (R5-2, ADR 0012 §3): the
-- destination path and git blob id of the write a record was last released
-- with as remote_write_unverified (an ambiguous write whose read-only check
-- did not finish, or a receipt the database could not record). The next
-- claim checks exactly this blob at this path, so changing github_path_prefix,
-- path templates or report templates in between cannot send the recovery to
-- another path and make it write the report a second time. Read only while
-- last_error_kind is remote_write_unverified; a stage advance and a charged
-- retryable release clear it. Existing rows: NULL (their recovery re-renders).
ALTER TABLE publish_records ADD COLUMN unverified_write_path TEXT NULL;
ALTER TABLE publish_records ADD COLUMN unverified_write_blob_sha TEXT NULL;
