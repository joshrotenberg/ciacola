ALTER TABLE repo_worker_assignments ADD COLUMN pr_state TEXT CHECK (
             pr_state IS NULL OR pr_state IN ('open', 'closed', 'merged'))