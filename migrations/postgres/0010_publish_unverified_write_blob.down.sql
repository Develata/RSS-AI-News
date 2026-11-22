-- Roll back the recorded unverified remote write.
ALTER TABLE publish_records DROP COLUMN unverified_write_blob_sha;
ALTER TABLE publish_records DROP COLUMN unverified_write_path;
