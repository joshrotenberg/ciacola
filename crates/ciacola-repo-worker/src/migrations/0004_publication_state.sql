ALTER TABLE repo_worker_assignments ADD COLUMN publication_state TEXT NOT NULL
             DEFAULT 'unpublished' CHECK (
                 publication_state IN ('unpublished', 'publishing', 'published', 'failed'))