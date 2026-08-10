ALTER TABLE repo_worker_assignments ADD COLUMN pr_draft INTEGER CHECK (
             pr_draft IS NULL OR pr_draft IN (0, 1))