ALTER TABLE repo_worker_assignments ADD COLUMN cleanup_reason TEXT CHECK (
             cleanup_reason IS NULL OR cleanup_reason IN ('absent', 'no_changes', 'merged', 'discarded'))