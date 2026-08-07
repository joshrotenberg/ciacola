//! Findings: introspection as a first-class feature.
//!
//! flat10 proved the loop informally: managers noticed a platform bug,
//! described it in prose, and the operator fixed it. This makes the
//! channel structural. A finding is an object with a kind (bug,
//! suggestion, observation), a subject, a body, and a status; agents
//! file them with a tool, the operator reads them on the board or the
//! `ciacola://findings` resource and resolves them applied or dismissed.
//!
//! The trajectory this is on, recorded in FINDINGS.md: during
//! development the queue accelerates the loop between what agents hit
//! and what the builders fix. Against a public repo, open findings
//! become the source for the system filing issues on itself, and,
//! with stage 7's worktrees, cloning itself to fix them.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::time::now_unix;

pub const KINDS: [&str; 3] = ["bug", "suggestion", "observation"];

#[derive(Clone)]
pub struct Findings {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct FindingRow {
    pub finding_id: String,
    pub kind: String,
    pub subject: String,
    pub body: String,
    pub author: Option<String>,
    pub status: String,
    pub resolution: Option<String>,
    pub created_unix: i64,
}

type Row = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    i64,
);

fn row(t: Row) -> FindingRow {
    let (finding_id, kind, subject, body, author, status, resolution, created_unix) = t;
    FindingRow {
        finding_id,
        kind,
        subject,
        body,
        author,
        status,
        resolution,
        created_unix,
    }
}

impl Findings {
    /// Wrap an already-migrated pool. Schema is the plugin's
    /// `migrations()`, not this constructor.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn setup(pool: SqlitePool) -> Result<Self, FlatError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS findings (
                 finding_id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 subject TEXT NOT NULL,
                 body TEXT NOT NULL,
                 author TEXT,
                 status TEXT NOT NULL DEFAULT 'open',
                 resolution TEXT,
                 created_unix INTEGER NOT NULL,
                 resolved_unix INTEGER)",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    pub async fn file(
        &self,
        kind: &str,
        subject: &str,
        body: &str,
        author: Option<&str>,
    ) -> Result<String, FlatError> {
        let finding_id = ulid::Ulid::new().to_string();
        sqlx::query(
            "INSERT INTO findings (finding_id, kind, subject, body, author, created_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&finding_id)
        .bind(kind)
        .bind(subject)
        .bind(body)
        .bind(author)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(finding_id)
    }

    pub async fn resolve(
        &self,
        finding_id: &str,
        status: &str,
        resolution: Option<&str>,
    ) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "UPDATE findings SET status = ?2, resolution = ?3, resolved_unix = ?4
             WHERE finding_id = ?1 AND status = 'open'",
        )
        .bind(finding_id)
        .bind(status)
        .bind(resolution)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Resolved findings older than the cutoff. Open ones are never
    /// dropped: an unanswered report is not garbage.
    pub async fn prune(&self, cutoff: i64) -> Result<u64, FlatError> {
        Ok(
            sqlx::query("DELETE FROM findings WHERE status <> 'open' AND resolved_unix < ?1")
                .bind(cutoff)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    pub async fn list(&self, status: Option<&str>) -> Result<Vec<FindingRow>, FlatError> {
        let rows: Vec<Row> = match status {
            Some(status) => {
                sqlx::query_as(
                    "SELECT finding_id, kind, subject, body, author, status, resolution,
                            created_unix
                     FROM findings WHERE status = ?1 ORDER BY created_unix DESC",
                )
                .bind(status)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT finding_id, kind, subject, body, author, status, resolution,
                            created_unix
                     FROM findings ORDER BY created_unix DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(row).collect())
    }
}

fn finding_json(f: &FindingRow) -> serde_json::Value {
    json!({
        "finding_id": f.finding_id,
        "kind": f.kind,
        "subject": f.subject,
        "body": f.body,
        "author": f.author,
        "status": f.status,
        "resolution": f.resolution,
        "created_unix": f.created_unix,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FileFindingArgs {
    /// bug, suggestion, or observation.
    kind: String,
    /// What it is about: "flat-server", "prompt:spoke-provisioning",
    /// "repo:tower-mcp", etc.
    subject: String,
    /// The finding itself: what was observed, why it matters, what to
    /// do about it. Self-contained; the reader has no other context.
    body: String,
    /// Your agent_id.
    author: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FindingsArgs {
    /// open, applied, or dismissed. Omit for all.
    status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResolveFindingArgs {
    /// The finding to resolve.
    finding_id: String,
    /// applied or dismissed.
    status: String,
    /// What was done about it, or why not.
    resolution: Option<String>,
}

/// The filing and reading tools, for both surfaces.
pub fn tools(findings: Findings) -> Vec<Tool> {
    let file = {
        let findings = findings.clone();
        ToolBuilder::new("file_finding")
            .description(
                "File a bug, suggestion, or observation about the system \
                 itself: platform defects you hit, prompt patterns that \
                 keep failing, config the operator should change. This \
                 is the introspection channel; it is read and acted on.",
            )
            .non_destructive()
            .handler(move |args: FileFindingArgs| {
                let findings = findings.clone();
                async move {
                    if !KINDS.contains(&args.kind.as_str()) {
                        return Ok(CallToolResult::error(format!(
                            "kind must be one of {KINDS:?}"
                        )));
                    }
                    match findings
                        .file(
                            &args.kind,
                            &args.subject,
                            &args.body,
                            args.author.as_deref(),
                        )
                        .await
                    {
                        Ok(finding_id) => {
                            Ok(CallToolResult::json(json!({ "finding_id": finding_id })))
                        }
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let list = {
        let findings = findings.clone();
        ToolBuilder::new("findings")
            .description("Findings filed about the system, optionally by status.")
            .read_only()
            .handler(move |args: FindingsArgs| {
                let findings = findings.clone();
                async move {
                    match findings.list(args.status.as_deref()).await {
                        Ok(all) => Ok(CallToolResult::json(json!({
                            "findings": all.iter().map(finding_json).collect::<Vec<_>>()
                        }))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    vec![file, list]
}

/// The operator's resolve tool: stdio surface only. Agents file;
/// people (or a future privileged agent) decide.
pub fn operator_tools(findings: Findings) -> Vec<Tool> {
    let resolve = ToolBuilder::new("resolve_finding")
        .description("Mark a finding applied or dismissed, with what was done.")
        .non_destructive()
        .handler(move |args: ResolveFindingArgs| {
            let findings = findings.clone();
            async move {
                if args.status != "applied" && args.status != "dismissed" {
                    return Ok(CallToolResult::error(
                        "status must be applied or dismissed".to_string(),
                    ));
                }
                match findings
                    .resolve(&args.finding_id, &args.status, args.resolution.as_deref())
                    .await
                {
                    Ok(true) => Ok(CallToolResult::json(json!({ "resolved": true }))),
                    Ok(false) => Ok(CallToolResult::error(format!(
                        "finding '{}' not open or not found",
                        args.finding_id
                    ))),
                    Err(e) => Ok(CallToolResult::error(e.to_string())),
                }
            }
        })
        .build();
    vec![resolve]
}

pub fn resources(findings: Findings) -> Vec<Resource> {
    let all = ResourceBuilder::new("ciacola://findings")
        .name("findings")
        .description("What the system has noticed about itself, newest first.")
        .mime_type("application/json")
        .handler(move || {
            let findings = findings.clone();
            async move {
                let rows = findings.list(None).await.unwrap_or_default();
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "ciacola://findings".to_string(),
                        mime_type: Some("application/json".to_string()),
                        text: Some(
                            json!(rows.iter().map(finding_json).collect::<Vec<_>>()).to_string(),
                        ),
                        blob: None,
                        meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build();
    vec![all]
}

// --- plugin ---

use ciacola_core::plugin::{BoxFut, Migration, Plugin, PluginContext, Section, Surface};

/// Findings as a plugin. `resolve_finding` is operator-only: agents
/// report what they notice, people decide what happens about it.
#[derive(Default)]
pub struct FindingsPlugin {
    findings: Option<Findings>,
}

impl FindingsPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for FindingsPlugin {
    fn tables(&self) -> &'static [&'static str] {
        &["findings"]
    }

    fn migrations(&self) -> &'static [Migration] {
        {
            const M: &[Migration] = &[Migration::new(
                "0001_findings",
                "CREATE TABLE IF NOT EXISTS findings (
                 finding_id TEXT PRIMARY KEY, kind TEXT NOT NULL, subject TEXT NOT NULL,
                 body TEXT NOT NULL, author TEXT, status TEXT NOT NULL DEFAULT 'open',
                 resolution TEXT, created_unix INTEGER NOT NULL, resolved_unix INTEGER);",
            )];
            M
        }
    }

    fn name(&self) -> &'static str {
        "findings"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.findings = Some(Findings::new(ctx.pool.clone()));
            Ok(())
        })
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        let Some(findings) = self.findings.as_ref() else {
            return Vec::new();
        };
        let mut all = tools(findings.clone());
        if surface == Surface::Operator {
            all.extend(operator_tools(findings.clone()));
        }
        all
    }

    fn resources(&self) -> Vec<Resource> {
        self.findings
            .as_ref()
            .map(|f| resources(f.clone()))
            .unwrap_or_default()
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let open = self.findings.as_ref()?.list(Some("open")).await.ok()?;
            if open.is_empty() {
                return None;
            }
            let mut html = String::from(
                "<table><tr><th>kind</th><th>subject</th><th>body</th><th>by</th></tr>",
            );
            for f in &open {
                html.push_str(&format!(
                    "<tr><td>{kind}</td><td>{subject}</td><td class=\"dim\">{body}</td>\
                     <td class=\"dim mono\">{by}</td></tr>",
                    kind = ciacola_core::board::esc(&f.kind),
                    subject = ciacola_core::board::esc(&f.subject),
                    body = ciacola_core::board::esc(&f.body.chars().take(180).collect::<String>()),
                    by = ciacola_core::board::esc(
                        f.author
                            .as_deref()
                            .map(|a| &a[a.len().saturating_sub(6)..])
                            .unwrap_or("-")
                    ),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "findings (open)".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(findings) = self.findings.as_ref() else {
                return json!({});
            };
            let all = findings.list(None).await.unwrap_or_default();
            let open = all.iter().filter(|f| f.status == "open").count();
            json!({ "open": open, "resolved": all.len() - open })
        })
    }

    fn prune(&self, cutoff: i64) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(findings) = self.findings.as_ref() else {
                return json!({});
            };
            match findings.prune(cutoff).await {
                Ok(n) => json!({ "resolved_deleted": n }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        })
    }
}
