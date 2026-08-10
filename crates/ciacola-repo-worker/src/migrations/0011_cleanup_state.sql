ALTER TABLE repo_worker_assignments ADD COLUMN cleanup_state TEXT NOT NULL
             DEFAULT 'none' CHECK (
                 cleanup_state IN ('none', 'retaining', 'retained', 'removing', 'completed', 'failed'))