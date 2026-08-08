//! Work items: the kanban, and the manager's externalized memory.
//!
//! Two identities, one table. For a person it is the done/doing/todo
//! view of what the system is up to. For the manager it is working
//! state that survives outside its context window: a fresh session can
//! reconstitute the backlog from a resource read instead of from
//! conversation memory, which is what makes session rotation cheap.
//!
//! The mechanics are mechanical (storage, rendering, the MCP surface);
//! the judgment of what sits in which lane belongs to the manager. The
//! read side is MCP *resources*, not tools: `ciacola://kanban` is data to
//! look at, has no side effects, and any MCP client can render or
//! subscribe to it. First real use of resources in this workspace.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;
use ciacola_core::time::now_unix;

pub const LANES: [&str; 4] = ["todo", "doing", "done", "dropped"];

#[derive(Clone)]
pub struct Items {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct ItemEvent {
    pub item_id: String,
    pub seq: i64,
    pub lane: String,
    pub owner: Option<String>,
    pub note: Option<String>,
    pub at_unix: i64,
}

#[derive(Debug, Clone)]
pub struct ItemRow {
    pub item_id: String,
    pub title: String,
    pub lane: String,
    pub owner: Option<String>,
    pub note: Option<String>,
    pub updated_unix: i64,
}

type Row = (String, String, String, Option<String>, Option<String>, i64);
type EventRow = (String, i64, String, Option<String>, Option<String>, i64);

fn row(t: Row) -> ItemRow {
    let (item_id, title, lane, owner, note, updated_unix) = t;
    ItemRow {
        item_id,
        title,
        lane,
        owner,
        note,
        updated_unix,
    }
}

impl Items {
    /// Wrap an already-migrated pool. Schema is the plugin's
    /// `migrations()`, not this constructor.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn setup(pool: SqlitePool) -> Result<Self, FlatError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS work_items (
                 item_id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 lane TEXT NOT NULL,
                 owner TEXT,
                 note TEXT,
                 updated_unix INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS item_events (
                 item_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 lane TEXT NOT NULL,
                 owner TEXT,
                 note TEXT,
                 at_unix INTEGER NOT NULL,
                 PRIMARY KEY (item_id, seq))",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    /// Upsert-and-move in one call. Title sticks from first track;
    /// owner and note are replaced (pass them each time they matter).
    pub async fn track(
        &self,
        item_id: &str,
        title: Option<&str>,
        lane: &str,
        owner: Option<&str>,
        note: Option<&str>,
    ) -> Result<ItemRow, FlatError> {
        sqlx::query(
            "INSERT INTO work_items (item_id, title, lane, owner, note, updated_unix)
             VALUES (?1, COALESCE(?2, ?1), ?3, ?4, ?5, ?6)
             ON CONFLICT(item_id) DO UPDATE SET
                 title = COALESCE(?2, title),
                 lane = ?3, owner = ?4, note = ?5, updated_unix = ?6",
        )
        .bind(item_id)
        .bind(title)
        .bind(lane)
        .bind(owner)
        .bind(note)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        // The work dimension is a history, not a state: every track is
        // also an event, so an item's journey (lanes, owners, notes,
        // corrections) is replayable on its own page.
        sqlx::query(
            "INSERT INTO item_events (item_id, seq, lane, owner, note, at_unix)
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM item_events
                          WHERE item_id = ?1), ?2, ?3, ?4, ?5)",
        )
        .bind(item_id)
        .bind(lane)
        .bind(owner)
        .bind(note)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(self.get(item_id).await?.expect("just upserted"))
    }

    /// Closed items older than the cutoff, and their events. Retention
    /// for this table belongs to this plugin.
    pub async fn prune(&self, cutoff: i64) -> Result<(u64, u64), FlatError> {
        let events = sqlx::query(
            "DELETE FROM item_events WHERE item_id IN
                 (SELECT item_id FROM work_items
                  WHERE lane IN ('done', 'dropped') AND updated_unix < ?1)",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        let items = sqlx::query(
            "DELETE FROM work_items WHERE lane IN ('done', 'dropped') AND updated_unix < ?1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok((items, events))
    }

    pub async fn events(&self, item_id: &str) -> Result<Vec<ItemEvent>, FlatError> {
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT item_id, seq, lane, owner, note, at_unix
                 FROM item_events WHERE item_id = ?1 ORDER BY seq",
        )
        .bind(item_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(item_id, seq, lane, owner, note, at_unix)| ItemEvent {
                item_id,
                seq,
                lane,
                owner,
                note,
                at_unix,
            })
            .collect())
    }

    pub async fn get(&self, item_id: &str) -> Result<Option<ItemRow>, FlatError> {
        let r: Option<Row> = sqlx::query_as(
            "SELECT item_id, title, lane, owner, note, updated_unix
             FROM work_items WHERE item_id = ?1",
        )
        .bind(item_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(r.map(row))
    }

    pub async fn list(&self, lane: Option<&str>) -> Result<Vec<ItemRow>, FlatError> {
        let rows: Vec<Row> = match lane {
            Some(lane) => {
                sqlx::query_as(
                    "SELECT item_id, title, lane, owner, note, updated_unix
                     FROM work_items WHERE lane = ?1 ORDER BY updated_unix DESC",
                )
                .bind(lane)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT item_id, title, lane, owner, note, updated_unix
                     FROM work_items ORDER BY updated_unix DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(row).collect())
    }
}

fn item_json(item: &ItemRow) -> serde_json::Value {
    json!({
        "item_id": item.item_id,
        "title": item.title,
        "lane": item.lane,
        "owner": item.owner,
        "note": item.note,
        "updated_unix": item.updated_unix,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TrackArgs {
    /// Stable id for the piece of work, e.g. "issue-1240".
    item_id: String,
    /// Human title. Required the first time, sticky after.
    title: Option<String>,
    /// Where the work stands.
    lane: Lane,
    /// The agent currently on it, if any.
    owner: Option<String>,
    /// Status note: why deferred, what a correction was, the outcome.
    note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ItemsArgs {
    /// Filter to one lane. Omit for all.
    lane: Option<Lane>,
}

/// A real enum rather than a documented string, so a client completes
/// it from the schema without asking the server anything.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Todo,
    Doing,
    Done,
    Dropped,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Todo => "todo",
            Lane::Doing => "doing",
            Lane::Done => "done",
            Lane::Dropped => "dropped",
        }
    }
}

/// The write side: `track`. The read side for agents that prefer a
/// tool: `items`. (The resource is the canonical read.)
pub fn tools(items: Items) -> Vec<Tool> {
    let track = {
        let items = items.clone();
        ToolBuilder::new("track")
            .description(
                "Put a work item in a lane (todo, doing, done, dropped), \
                 creating it if new. This is the kanban and the durable \
                 memory of what is in flight; track every decision.",
            )
            .non_destructive()
            .handler(move |args: TrackArgs| {
                let items = items.clone();
                async move {
                    match items
                        .track(
                            &args.item_id,
                            args.title.as_deref(),
                            args.lane.as_str(),
                            args.owner.as_deref(),
                            args.note.as_deref(),
                        )
                        .await
                    {
                        Ok(item) => Ok(CallToolResult::json(item_json(&item))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let list = ToolBuilder::new("items")
        .description("Work items, optionally filtered to one lane.")
        .read_only()
        .handler(move |args: ItemsArgs| {
            let items = items.clone();
            async move {
                match items.list(args.lane.map(Lane::as_str)).await {
                    Ok(all) => Ok(CallToolResult::json(json!({
                        "items": all.iter().map(item_json).collect::<Vec<_>>()
                    }))),
                    Err(e) => Ok(CallToolResult::error(e.to_string())),
                }
            }
        })
        .build();

    vec![track, list]
}

fn json_resource(uri: &str, value: serde_json::Value) -> ReadResourceResult {
    ReadResourceResult {
        contents: vec![ResourceContent {
            uri: uri.to_string(),
            mime_type: Some("application/json".to_string()),
            text: Some(value.to_string()),
            blob: None,
            meta: None,
        }],
        ..Default::default()
    }
}

/// The read surface: mechanical views over the ledger, as resources.
/// Call once per router; resources are cheap to rebuild.
pub fn resources_for(items: Items) -> Vec<Resource> {
    let kanban = {
        let items = items.clone();
        ResourceBuilder::new("ciacola://kanban")
            .name("kanban")
            .description(
                "Work items by lane: todo, doing, done, dropped. The \
                 board's kanban and the manager's durable memory.",
            )
            .mime_type("application/json")
            .handler(move || {
                let items = items.clone();
                async move {
                    let all = items.list(None).await.unwrap_or_default();
                    let lanes: serde_json::Map<String, serde_json::Value> = LANES
                        .iter()
                        .map(|lane| {
                            (
                                lane.to_string(),
                                json!(
                                    all.iter()
                                        .filter(|i| i.lane == *lane)
                                        .map(item_json)
                                        .collect::<Vec<_>>()
                                ),
                            )
                        })
                        .collect();
                    Ok(json_resource("ciacola://kanban", json!(lanes)))
                }
            })
            .build()
    };

    vec![kanban]
}

/// The agents resource lives in core, not here: it reads the ledger.
pub fn agents_resource(ledger: Ledger) -> Resource {
    ResourceBuilder::new("ciacola://agents")
        .name("agents")
        .description("Every active agent: state, turns, cost, lineage.")
        .mime_type("application/json")
        .handler(move || {
            let ledger = ledger.clone();
            async move {
                let all = ledger.list_agents().await.unwrap_or_default();
                let view = all
                    .iter()
                    .map(|a| {
                        json!({
                            "agent_id": a.agent_id,
                            "name": a.name,
                            "state": a.state,
                            "turns": a.turns,
                            "cost_usd": a.cost_micro_usd as f64 / 1e6,
                            "spawned_by": a.spawned_by,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json_resource("ciacola://agents", json!(view)))
            }
        })
        .build()
}

// --- plugin ---

use ciacola_core::plugin::{BoxFut, Migration, Plugin, PluginContext, Section, Surface};

/// The kanban as a plugin. Contributes tools, a resource, the work
/// columns on the board, its own size stats, and its own retention:
/// closed items and their events are its business, not the host's.
#[derive(Default)]
pub struct KanbanPlugin {
    items: Option<Items>,
    ledger: Option<Ledger>,
}

impl KanbanPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    fn items(&self) -> Option<&Items> {
        self.items.as_ref()
    }
}

impl Plugin for KanbanPlugin {
    fn tables(&self) -> &'static [&'static str] {
        &["work_items", "item_events"]
    }

    fn migrations(&self) -> &'static [Migration] {
        {
            const M: &[Migration] = &[Migration::new(
                "0001_work_items",
                "CREATE TABLE IF NOT EXISTS work_items (
                 item_id TEXT PRIMARY KEY, title TEXT NOT NULL, lane TEXT NOT NULL,
                 owner TEXT, note TEXT, updated_unix INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS item_events (
                 item_id TEXT NOT NULL, seq INTEGER NOT NULL, lane TEXT NOT NULL,
                 owner TEXT, note TEXT, at_unix INTEGER NOT NULL,
                 PRIMARY KEY (item_id, seq));",
            )];
            M
        }
    }

    fn name(&self) -> &'static str {
        "kanban"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.items = Some(Items::new(ctx.pool.clone()));
            self.ledger = Some(ctx.ledger.clone());
            Ok(())
        })
    }

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        self.items().map(|i| tools(i.clone())).unwrap_or_default()
    }

    fn resources(&self) -> Vec<Resource> {
        self.items()
            .map(|i| resources_for(i.clone()))
            .unwrap_or_default()
    }

    fn routes(&self) -> Option<Router> {
        Some(item_routes(self.items()?.clone(), self.ledger.clone()?))
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let items = self.items()?;
            let all = items.list(None).await.unwrap_or_default();
            if all.is_empty() {
                return None;
            }
            let mut html = String::from("<div class=\"kanban\">");
            for lane in LANES {
                html.push_str(&format!("<div class=\"lane\"><h3>{lane}</h3>"));
                for item in all.iter().filter(|i| i.lane == lane) {
                    html.push_str(&format!(
                        "<div class=\"card\"><b><a href=\"/board/item/{iid}\">{title}</a></b>\
                         <span class=\"dim mono\">{iid}{owner}</span>{note}</div>",
                        iid = ciacola_core::render::esc(&item.item_id),
                        title = ciacola_core::render::esc(&item.title),
                        owner = item
                            .owner
                            .as_deref()
                            .map(|o| format!(
                                " &middot; {}",
                                ciacola_core::render::esc(&o[o.len().saturating_sub(6)..])
                            ))
                            .unwrap_or_default(),
                        note = item
                            .note
                            .as_deref()
                            .map(|n| format!(
                                "<span class=\"dim\">{}</span>",
                                ciacola_core::render::esc(&n.chars().take(160).collect::<String>())
                            ))
                            .unwrap_or_default(),
                    ));
                }
                html.push_str("</div>");
            }
            html.push_str("</div>");
            Some(Section {
                title: "work".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(items) = self.items() else {
                return json!({});
            };
            let all = items.list(None).await.unwrap_or_default();
            let closed = all
                .iter()
                .filter(|i| i.lane == "done" || i.lane == "dropped")
                .count();
            json!({ "items": all.len(), "closed": closed })
        })
    }

    fn prune(&self, cutoff: i64) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(items) = self.items() else {
                return json!({});
            };
            match items.prune(cutoff).await {
                Ok((items_deleted, events_deleted)) => json!({
                    "items_deleted": items_deleted,
                    "item_events_deleted": events_deleted,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        })
    }
}

/// The kanban's own board page: one item's journey, who worked it, and
/// what it cost. Owned here rather than threaded through core, which
/// is what `board_routes` is for.
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Html;
use axum::routing::get;

use ciacola_core::render;

async fn item_page(
    State((items, ledger)): State<(Items, Ledger)>,
    Path(item_id): Path<String>,
) -> Html<String> {
    let Ok(Some(item)) = items.get(&item_id).await else {
        return render::page_with(
            "not found",
            "<p>no such item. <a href=\"/board\">back</a></p>",
            false,
        );
    };
    let events = items.events(&item_id).await.unwrap_or_default();

    // Cost attribution: every agent that ever owned this item, with its
    // whole cost. Rough on shared spokes, honest about what work costs;
    // the manager's slice is unattributable today and says so.
    let mut owners: Vec<String> = Vec::new();
    for event in &events {
        if let Some(owner) = &event.owner {
            if !owners.contains(owner) {
                owners.push(owner.clone());
            }
        }
    }
    let mut attributed: i64 = 0;
    let mut agents_html = String::new();
    for owner in &owners {
        if let Ok(Some(agent)) = ledger.get_agent(owner).await {
            attributed += agent.cost_micro_usd;
            agents_html.push_str(&format!(
                "<tr><td><a href=\"/board/agent/{id}\">{name}</a></td><td>{chip}</td>\
                 <td class=\"num\">{turns}</td><td class=\"num\">{cost}</td></tr>",
                id = render::esc(&agent.agent_id),
                name = render::esc(&agent.name),
                chip = render::chip(&agent.state),
                turns = agent.turns,
                cost = render::usd(agent.cost_micro_usd),
            ));
        }
    }

    let mut body = format!(
        "<p><a href=\"/board\">&larr; board</a></p>\
         <h1>{title} {chip}</h1>\
         <p class=\"dim mono\">{id}</p>\
         <p class=\"dim\">attributed cost {cost} across {n} agent(s), plus an \
          unattributed share of the manager's turns</p>",
        title = render::esc(&item.title),
        chip = render::chip(&item.lane),
        id = render::esc(&item.item_id),
        cost = render::usd(attributed),
        n = owners.len(),
    );

    if !agents_html.is_empty() {
        body.push_str(&format!(
            "<h2>worked by</h2><table><tr><th>agent</th><th>state</th>\
             <th class=\"num\">turns</th><th class=\"num\">cost</th></tr>{agents_html}</table>"
        ));
    }

    body.push_str("<h2>journey</h2>");
    for event in &events {
        body.push_str(&format!(
            "<h2>{seq}. {chip} <span class=\"dim\">{owner}</span></h2>\
             <div class=\"msg them\">{note}</div>",
            seq = event.seq,
            chip = render::chip(&event.lane),
            owner = event
                .owner
                .as_deref()
                .map(|o| format!("owner ..{}", &o[o.len().saturating_sub(6)..]))
                .unwrap_or_default(),
            note = render::esc(event.note.as_deref().unwrap_or("(no note)")),
        ));
    }
    render::page_with(&item.title, &body, false)
}

fn item_routes(items: Items, ledger: Ledger) -> Router {
    Router::new()
        .route("/board/item/{item_id}", get(item_page))
        .with_state((items, ledger))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn items() -> Items {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        Items::setup(pool).await.expect("setup")
    }

    #[tokio::test]
    async fn track_without_title_defaults_to_item_id() {
        let items = items().await;
        let item = items
            .track("issue-1", None, "todo", None, None)
            .await
            .expect("track");
        assert_eq!(item.title, "issue-1");
    }

    #[tokio::test]
    async fn track_update_without_title_keeps_the_original() {
        let items = items().await;
        items
            .track("issue-1", Some("Fix the thing"), "todo", None, None)
            .await
            .expect("create");
        let item = items
            .track("issue-1", None, "doing", None, None)
            .await
            .expect("move without a title");
        assert_eq!(item.title, "Fix the thing");
        assert_eq!(item.lane, "doing");
    }

    #[tokio::test]
    async fn track_explicit_title_wins_on_update() {
        let items = items().await;
        items
            .track("issue-1", Some("Fix the thing"), "todo", None, None)
            .await
            .expect("create");
        let item = items
            .track("issue-1", Some("Fix the other thing"), "doing", None, None)
            .await
            .expect("update with a title");
        assert_eq!(item.title, "Fix the other thing");
    }

    #[tokio::test]
    async fn event_sequence_counts_per_item() {
        let items = items().await;
        items
            .track("issue-1", Some("a"), "todo", None, None)
            .await
            .expect("first track");
        items
            .track("issue-1", None, "doing", None, None)
            .await
            .expect("second track");
        let events = items.events("issue-1").await.expect("events");
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[tokio::test]
    async fn event_sequence_restarts_for_a_different_item() {
        let items = items().await;
        items
            .track("issue-1", Some("a"), "todo", None, None)
            .await
            .expect("track issue-1");
        items
            .track("issue-1", None, "doing", None, None)
            .await
            .expect("track issue-1 again");
        items
            .track("issue-2", Some("b"), "todo", None, None)
            .await
            .expect("track issue-2");

        let first_item_events = items.events("issue-1").await.expect("events for issue-1");
        assert_eq!(
            first_item_events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let second_item_events = items.events("issue-2").await.expect("events for issue-2");
        assert_eq!(
            second_item_events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[tokio::test]
    async fn list_filters_by_lane() {
        let items = items().await;
        items
            .track("issue-1", Some("a"), "todo", None, None)
            .await
            .expect("track a");
        items
            .track("issue-2", Some("b"), "doing", None, None)
            .await
            .expect("track b");
        items
            .track("issue-3", Some("c"), "todo", None, None)
            .await
            .expect("track c");

        let todo = items.list(Some("todo")).await.expect("list todo");
        assert_eq!(todo.len(), 2);
        assert!(todo.iter().all(|i| i.lane == "todo"));

        let all = items.list(None).await.expect("list all");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn lane_rejects_a_value_outside_the_enum() {
        let err = serde_json::from_value::<TrackArgs>(json!({
            "item_id": "issue-1",
            "lane": "blocked",
        }))
        .expect_err("\"blocked\" is not a lane");
        assert!(err.to_string().contains("blocked"), "{err}");
    }
}
