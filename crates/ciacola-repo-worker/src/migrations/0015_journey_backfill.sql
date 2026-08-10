UPDATE repo_worker_assignments
            SET publication_state = 'published'
          WHERE pr IS NOT NULL;
         UPDATE repo_worker_assignments
            SET cleanup_state = CASE state
                WHEN 'finishing' THEN CASE
                    WHEN phase = 'finishing_keep' THEN 'retaining'
                    ELSE 'removing'
                END
                WHEN 'retained' THEN 'retained'
                WHEN 'completed' THEN 'completed'
                WHEN 'stale' THEN 'failed'
                ELSE 'none'
            END