//! The assignments schema, one recorded change at a time.
//!
//! Each migration's SQL lives in `migrations/<name>.sql`, named
//! exactly after its durable identifier. Names are recorded once
//! applied and never renamed; recorded migrations never re-execute,
//! so the files matter only to fresh databases.

use ciacola_core::plugin::Migration;

pub(crate) const ASSIGNMENTS_TABLE: &str = "repo_worker_assignments";
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration::new(
        "0001_assignments",
        include_str!("migrations/0001_assignments.sql"),
    ),
    Migration::add_column(
        "0002_base_head",
        include_str!("migrations/0002_base_head.sql"),
    ),
    Migration::add_column(
        "0003_expected_head",
        include_str!("migrations/0003_expected_head.sql"),
    ),
    Migration::add_column(
        "0004_publication_state",
        include_str!("migrations/0004_publication_state.sql"),
    ),
    Migration::add_column("0005_pr_url", include_str!("migrations/0005_pr_url.sql")),
    Migration::add_column(
        "0006_pr_state",
        include_str!("migrations/0006_pr_state.sql"),
    ),
    Migration::add_column(
        "0007_pr_draft",
        include_str!("migrations/0007_pr_draft.sql"),
    ),
    Migration::add_column("0008_pr_head", include_str!("migrations/0008_pr_head.sql")),
    Migration::add_column("0009_pr_base", include_str!("migrations/0009_pr_base.sql")),
    Migration::add_column(
        "0010_pr_checked_unix",
        include_str!("migrations/0010_pr_checked_unix.sql"),
    ),
    Migration::add_column(
        "0011_cleanup_state",
        include_str!("migrations/0011_cleanup_state.sql"),
    ),
    Migration::add_column(
        "0012_cleanup_head",
        include_str!("migrations/0012_cleanup_head.sql"),
    ),
    Migration::add_column(
        "0013_cleanup_reason",
        include_str!("migrations/0013_cleanup_reason.sql"),
    ),
    Migration::add_column(
        "0014_pushed_head",
        include_str!("migrations/0014_pushed_head.sql"),
    ),
    Migration::new(
        "0015_journey_backfill",
        include_str!("migrations/0015_journey_backfill.sql"),
    ),
    Migration::add_column(
        "0016_branch_policy",
        include_str!("migrations/0016_branch_policy.sql"),
    ),
];
