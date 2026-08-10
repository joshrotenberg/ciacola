ALTER TABLE repo_worker_assignments ADD COLUMN branch_policy TEXT NOT NULL
             DEFAULT 'agent/{slug}'