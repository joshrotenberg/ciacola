CREATE TABLE IF NOT EXISTS repo_worker_assignments (
         assignment_id TEXT PRIMARY KEY,
         repo TEXT NOT NULL COLLATE NOCASE,
         issue_number INTEGER NOT NULL,
         state TEXT NOT NULL CHECK (
             state IN ('preparing', 'active', 'finishing', 'retained', 'completed', 'stale')),
         phase TEXT NOT NULL,
         base TEXT,
         slug TEXT NOT NULL,
         branch TEXT NOT NULL,
         worktree TEXT NOT NULL,
         bare_path TEXT NOT NULL,
         agent_id TEXT UNIQUE,
         related_agent_ids TEXT NOT NULL DEFAULT '[]',
         spawned_by TEXT,
         pr INTEGER,
         last_error TEXT,
         created_unix INTEGER NOT NULL,
         updated_unix INTEGER NOT NULL,
         terminal_unix INTEGER,
         UNIQUE(repo, issue_number));
     CREATE UNIQUE INDEX IF NOT EXISTS repo_worker_owned_worktree
         ON repo_worker_assignments(worktree)
         WHERE state IN ('preparing', 'active', 'finishing', 'retained');
     CREATE UNIQUE INDEX IF NOT EXISTS repo_worker_owned_branch
         ON repo_worker_assignments(repo, branch)
         WHERE state IN ('preparing', 'active', 'finishing', 'retained');