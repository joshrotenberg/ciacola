//! Reference material: the shape of a plugin that writes no SQL.
//!
//! Things worth keeping a pointer to (docs, specs, an upstream issue,
//! a design note) so agents and people can find them again. Useful on
//! its own, and here mainly as the proof that [`Store`] is enough for
//! a real plugin: this file declares **no tables and no migrations**,
//! and still contributes tools, a resource, a board section, health,
//! and retention.
//!
//! Compare `items.rs`, which needs UPSERT with COALESCE, a
//! `MAX(seq) + 1` allocation, and a subquery DELETE, and so takes the
//! pool. Both are first-class; the difference is only how much the
//! plugin needs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::plugin::{BoxFut, Plugin, PluginContext, Section, Surface};
use ciacola_core::store::Store;

const KEY_PREFIX: &str = "ref/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub url: String,
    pub note: Option<String>,
    pub tags: Vec<String>,
    pub author: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveRefArgs {
    /// Short handle, unique. Overwrites if it exists.
    name: String,
    /// Where the thing is: a URL, a file path, an issue reference.
    url: String,
    /// Why it is worth keeping.
    note: Option<String>,
    /// For filtering, e.g. ["tower-mcp", "spec"].
    #[serde(default)]
    tags: Vec<String>,
    /// Your agent_id.
    author: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RefsArgs {
    /// Only references carrying this tag. Omit for all.
    tag: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ForgetRefArgs {
    /// The reference to drop.
    name: String,
}

fn ref_json(name: &str, r: &Reference) -> serde_json::Value {
    json!({
        "name": name,
        "url": r.url,
        "note": r.note,
        "tags": r.tags,
        "author": r.author,
    })
}

#[derive(Default)]
pub struct RefsPlugin {
    store: Option<Store>,
}

impl RefsPlugin {
    async fn all(&self) -> Vec<(String, Reference)> {
        match &self.store {
            Some(store) => store
                .list::<Reference>(Some(KEY_PREFIX))
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(key, r)| (key.trim_start_matches(KEY_PREFIX).to_string(), r))
                .collect(),
            None => Vec::new(),
        }
    }
}

impl Plugin for RefsPlugin {
    fn name(&self) -> &'static str {
        "refs"
    }

    // No tables(), no migrations(): the store's table is core's.

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.store = Some(Store::new(ctx.pool.clone(), self.name()));
            Ok(())
        })
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        let Some(store) = self.store.clone() else {
            return Vec::new();
        };

        let save = {
            let store = store.clone();
            ToolBuilder::new("save_ref")
                .description(
                    "Keep a pointer to something worth finding again: \
                     docs, a spec, an upstream issue, a design note.",
                )
                .non_destructive()
                .handler(move |args: SaveRefArgs| {
                    let store = store.clone();
                    async move {
                        let reference = Reference {
                            name: args.name.clone(),
                            url: args.url,
                            note: args.note,
                            tags: args.tags,
                            author: args.author,
                        };
                        match store
                            .put(&format!("{KEY_PREFIX}{}", args.name), &reference)
                            .await
                        {
                            Ok(()) => Ok(CallToolResult::json(json!({ "saved": args.name }))),
                            Err(e) => Ok(CallToolResult::error(e.to_string())),
                        }
                    }
                })
                .build()
        };

        let list = {
            let store = store.clone();
            ToolBuilder::new("refs")
                .description("Saved reference material, optionally filtered by tag.")
                .read_only()
                .handler(move |args: RefsArgs| {
                    let store = store.clone();
                    async move {
                        match store.list::<Reference>(Some(KEY_PREFIX)).await {
                            Ok(all) => Ok(CallToolResult::json(json!({
                                "refs": all
                                    .iter()
                                    .filter(|(_, r)| args
                                        .tag
                                        .as_ref()
                                        .is_none_or(|t| r.tags.contains(t)))
                                    .map(|(key, r)| ref_json(
                                        key.trim_start_matches(KEY_PREFIX),
                                        r
                                    ))
                                    .collect::<Vec<_>>()
                            }))),
                            Err(e) => Ok(CallToolResult::error(e.to_string())),
                        }
                    }
                })
                .build()
        };

        let mut tools = vec![save, list];
        if surface == Surface::Operator {
            tools.push(
                ToolBuilder::new("forget_ref")
                    .description("Drop a saved reference.")
                    .destructive()
                    .handler(move |args: ForgetRefArgs| {
                        let store = store.clone();
                        async move {
                            match store.delete(&format!("{KEY_PREFIX}{}", args.name)).await {
                                Ok(true) => {
                                    Ok(CallToolResult::json(json!({ "forgot": args.name })))
                                }
                                Ok(false) => Ok(CallToolResult::error(format!(
                                    "no reference '{}'",
                                    args.name
                                ))),
                                Err(e) => Ok(CallToolResult::error(e.to_string())),
                            }
                        }
                    })
                    .build(),
            );
        }
        tools
    }

    fn resources(&self) -> Vec<Resource> {
        let Some(store) = self.store.clone() else {
            return Vec::new();
        };
        vec![
            ResourceBuilder::new("ciacola://refs")
                .name("refs")
                .description("Reference material the system has been told to remember.")
                .mime_type("application/json")
                .handler(move || {
                    let store = store.clone();
                    async move {
                        let all = store
                            .list::<Reference>(Some(KEY_PREFIX))
                            .await
                            .unwrap_or_default();
                        Ok(ReadResourceResult {
                            contents: vec![ResourceContent {
                                uri: "ciacola://refs".to_string(),
                                mime_type: Some("application/json".to_string()),
                                text: Some(
                                    json!(
                                        all.iter()
                                            .map(|(key, r)| ref_json(
                                                key.trim_start_matches(KEY_PREFIX),
                                                r
                                            ))
                                            .collect::<Vec<_>>()
                                    )
                                    .to_string(),
                                ),
                                blob: None,
                                meta: None,
                            }],
                            ..Default::default()
                        })
                    }
                })
                .build(),
        ]
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let all = self.all().await;
            if all.is_empty() {
                return None;
            }
            let mut html = String::from(
                "<table><tr><th>name</th><th>where</th><th>why</th><th>tags</th></tr>",
            );
            for (name, r) in &all {
                html.push_str(&format!(
                    "<tr><td>{name}</td><td class=\"mono dim\">{url}</td>\
                     <td class=\"dim\">{note}</td><td class=\"dim\">{tags}</td></tr>",
                    name = ciacola_core::render::esc(name),
                    url = ciacola_core::render::esc(&r.url),
                    note = ciacola_core::render::esc(r.note.as_deref().unwrap_or("")),
                    tags = ciacola_core::render::esc(&r.tags.join(", ")),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "references".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(store) = self.store.as_ref() else {
                return json!({});
            };
            json!({
                "refs": store.count().await.unwrap_or_default(),
                "bytes": store.bytes().await.unwrap_or_default(),
            })
        })
    }

    // Deliberately no prune: a reference is a pointer someone chose to
    // keep, and age is not evidence it stopped mattering.
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    /// `plugin_kv` is core's table, applied by `PluginHost::setup`
    /// before any plugin runs. Duplicated here rather than dragged in
    /// through a full `PluginContext`, the same shortcut `Memory::setup`
    /// and `Findings::setup` take for their own tables in their tests.
    async fn kv_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS plugin_kv (
                 plugin TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 updated_unix INTEGER NOT NULL,
                 PRIMARY KEY (plugin, key));",
        )
        .execute(&pool)
        .await
        .expect("create plugin_kv");
        pool
    }

    fn refs_plugin(pool: SqlitePool) -> RefsPlugin {
        RefsPlugin {
            store: Some(Store::new(pool, "refs")),
        }
    }

    fn find<'a>(tools: &'a [Tool], name: &str) -> &'a Tool {
        tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("tool '{name}' registered"))
    }

    fn some_reference() -> Reference {
        Reference {
            name: "spec".into(),
            url: "https://elsewhere.example/spec".into(),
            note: None,
            tags: vec![],
            author: None,
        }
    }

    #[tokio::test]
    async fn a_saved_ref_is_invisible_to_another_plugins_store() {
        let pool = kv_pool().await;
        let refs = refs_plugin(pool.clone());
        let tools = refs.tools(Surface::Operator);
        let save = find(&tools, "save_ref");

        let result = save
            .call(json!({"name": "spec", "url": "https://example.com/spec"}))
            .await;
        assert!(!result.is_error, "{result:?}");

        // Same table, a different plugin's namespace: the key format is
        // shared, so only the plugin scope keeps it from leaking.
        let other = Store::new(pool, "other-plugin");
        assert!(
            other
                .list::<Reference>(Some(KEY_PREFIX))
                .await
                .expect("list")
                .is_empty(),
            "a ref saved by 'refs' must not be visible under another plugin's Store"
        );
    }

    #[tokio::test]
    async fn a_key_written_by_another_plugin_does_not_surface_as_a_ref() {
        let pool = kv_pool().await;
        let other = Store::new(pool.clone(), "other-plugin");
        other
            .put(&format!("{KEY_PREFIX}spec"), &some_reference())
            .await
            .expect("put");

        let refs = refs_plugin(pool);
        let tools = refs.tools(Surface::Operator);
        let list = find(&tools, "refs");

        let result = list.call(json!({})).await;
        assert!(!result.is_error, "{result:?}");
        let value = result.as_json().expect("json").expect("valid json");
        assert_eq!(
            value["refs"],
            json!([]),
            "a key under the same name but a different plugin's Store must not appear"
        );
    }

    #[tokio::test]
    async fn save_ref_round_trips_the_tags_array_and_replaces_on_resave() {
        let pool = kv_pool().await;
        let refs = refs_plugin(pool);
        let tools = refs.tools(Surface::Operator);
        let save = find(&tools, "save_ref");
        let list = find(&tools, "refs");

        let saved = save
            .call(json!({
                "name": "spec",
                "url": "https://example.com/spec",
                "tags": ["a", "b", "c"],
            }))
            .await;
        assert!(!saved.is_error, "{saved:?}");

        let got = list.call(json!({})).await;
        let value = got.as_json().expect("json").expect("valid json");
        assert_eq!(
            value["refs"][0]["tags"],
            json!(["a", "b", "c"]),
            "the tags array's order and contents must survive the round trip"
        );

        // Save again under the same name, with a different tag list.
        let resaved = save
            .call(json!({
                "name": "spec",
                "url": "https://example.com/spec-v2",
                "tags": ["z"],
            }))
            .await;
        assert!(!resaved.is_error, "{resaved:?}");

        let got = list.call(json!({})).await;
        let value = got.as_json().expect("json").expect("valid json");
        let all = value["refs"].as_array().expect("array");
        assert_eq!(
            all.len(),
            1,
            "save_ref under a name that already exists must replace it, not add a second entry"
        );
        assert_eq!(
            all[0]["tags"],
            json!(["z"]),
            "the second save replaces the tag list outright; it does not merge with the first"
        );
        assert_eq!(all[0]["url"], "https://example.com/spec-v2");
    }

    #[tokio::test]
    async fn forget_ref_distinguishes_absent_from_removed() {
        let pool = kv_pool().await;
        let refs = refs_plugin(pool);
        let tools = refs.tools(Surface::Operator);
        let save = find(&tools, "save_ref");
        let forget = find(&tools, "forget_ref");

        let missing = forget.call(json!({"name": "spec"})).await;
        assert!(
            missing.is_error,
            "forgetting a name that was never saved must be an error"
        );

        save.call(json!({"name": "spec", "url": "https://example.com/spec"}))
            .await;

        let removed = forget.call(json!({"name": "spec"})).await;
        assert!(
            !removed.is_error,
            "forgetting a name that exists must succeed"
        );

        let removed_again = forget.call(json!({"name": "spec"})).await;
        assert!(
            removed_again.is_error,
            "forgetting an already-removed name must again be distinguishable as absent"
        );
    }
}
