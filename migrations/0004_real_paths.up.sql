-- Files now live under real names on disk (<data>/<uid>/files/<path>); no blob ids any more.
ALTER TABLE file_cache DROP COLUMN storage_id;
