//! The one-shot CLI: the operator surface as subcommands, derived at
//! startup from the server's own `tools/list`.
//!
//! There is no second interface to drift. The command tree is built
//! from the same router `serve` attaches to stdio, called in-process
//! through its `Service<RouterRequest>` implementation, so a plugin
//! that adds a tool gets a subcommand and its help text for free.
//!
//! One-shot never serves: dispatch stays closed, so a queued turn is
//! left queued for a running server to claim rather than executed by a
//! process someone might Ctrl-C mid-provider-call, and the published
//! loopback config files of a running server are never overwritten.
//! `send` therefore always queues and says so; `--wait` waits for a
//! server to settle the turn and reports honestly when none does.

use std::ffi::OsString;

use clap::{Arg, ArgAction, ArgMatches, Command, builder::PossibleValuesParser};
use serde_json::{Map, Value, json};
use tower::Service;
use tower_mcp::McpRouter;
use tower_mcp::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, Content, Implementation, InitializeParams,
    ListToolsParams, McpNotification, McpRequest, McpResponse, RequestId, ToolDefinition,
};

use crate::{Mode, assemble, config, operator_auth, provider_auth};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Parse argv against the derived tree, run one tool call, print the
/// result, and return the process exit code.
pub(crate) async fn run_cli(
    argv: Vec<OsString>,
    operator_token: Option<operator_auth::HumanOperatorToken>,
    provider_credentials: provider_auth::ProviderCredentials,
    child_environment: ciacola_agent::ProviderChildEnvironment,
    config_path: Option<String>,
    declared: config::Config,
) -> Result<i32, Error> {
    let assembled = assemble(
        operator_token.as_ref(),
        provider_credentials,
        child_environment,
        config_path,
        declared,
        Mode::OneShot,
    )
    .await?;
    let mut router = assembled.stdio_router;

    initialize(&mut router).await?;
    let tools = list_tools(&mut router).await?;
    let command = derive_command(&tools);
    let matches = match command.try_get_matches_from(&argv) {
        Ok(matches) => matches,
        Err(rendered) => {
            // clap renders --help and usage errors itself; its exit
            // semantics (0 for help, 2 for errors) are the convention.
            let code = if rendered.use_stderr() { 2 } else { 0 };
            rendered.print()?;
            return Ok(code);
        }
    };
    let Some((subcommand, sub_matches)) = matches.subcommand() else {
        return Ok(2);
    };
    let tool_name = subcommand.replace('-', "_");
    let Some(tool) = tools.iter().find(|tool| tool.name == tool_name) else {
        eprintln!("no such tool: {tool_name}");
        return Ok(2);
    };

    let arguments = arguments_from_matches(tool, sub_matches)?;
    let result = call_tool(&mut router, &tool.name, arguments).await?;
    let mut failed = print_result(&result);

    // `send --wait` composes the wait tool onto a successful send, so a
    // shell one-liner can block on the turn a running server executes.
    if !failed && tool_name == "send" && sub_matches.get_flag("wait") {
        let seq = result
            .content
            .iter()
            .find_map(|content| match content {
                Content::Text { text, .. } => serde_json::from_str::<Value>(text).ok(),
                _ => None,
            })
            .and_then(|value| value.get("seq").and_then(Value::as_i64));
        match seq {
            Some(seq) => {
                let timeout = sub_matches.get_one::<i64>("wait-timeout-secs").copied();
                let mut wait_args = Map::new();
                wait_args.insert(
                    "agent_id".into(),
                    json!(sub_matches.get_one::<String>("agent_id").cloned()),
                );
                wait_args.insert("seq".into(), json!(seq));
                if let Some(timeout) = timeout {
                    wait_args.insert("timeout_secs".into(), json!(timeout));
                }
                let waited = call_tool(&mut router, "wait", Value::Object(wait_args)).await?;
                failed = print_result(&waited);
            }
            None => {
                eprintln!("send returned no seq to wait on");
                failed = true;
            }
        }
    }

    Ok(if failed { 1 } else { 0 })
}

/// The router enforces the MCP handshake for every caller, in-process
/// included; one initialize plus the initialized notification opens it.
async fn initialize(router: &mut McpRouter) -> Result<(), Error> {
    let params = InitializeParams {
        protocol_version: "2025-06-18".into(),
        capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: "ciacola-cli".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
            meta: None,
        },
        meta: None,
    };
    let response = router
        .call(tower_mcp::RouterRequest::new(
            RequestId::Number(0),
            McpRequest::Initialize(params),
        ))
        .await
        .expect("router error type is Infallible");
    match response.inner {
        Ok(McpResponse::Initialize(_)) => {
            router.handle_notification(McpNotification::Initialized);
            Ok(())
        }
        Ok(other) => Err(format!("unexpected initialize response: {other:?}").into()),
        Err(error) => Err(format!("initialize failed: {}", error.message).into()),
    }
}

async fn list_tools(router: &mut McpRouter) -> Result<Vec<ToolDefinition>, Error> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = ListToolsParams {
            cursor: cursor.take(),
            ..Default::default()
        };
        let response = router
            .call(tower_mcp::RouterRequest::new(
                RequestId::Number(1),
                McpRequest::ListTools(params),
            ))
            .await
            .expect("router error type is Infallible");
        match response.inner {
            Ok(McpResponse::ListTools(page)) => {
                tools.extend(page.tools);
                if page.next_cursor.is_none() {
                    return Ok(tools);
                }
                cursor = page.next_cursor;
            }
            Ok(other) => return Err(format!("unexpected tools/list response: {other:?}").into()),
            Err(error) => return Err(format!("tools/list failed: {}", error.message).into()),
        }
    }
}

async fn call_tool(
    router: &mut McpRouter,
    name: &str,
    arguments: Value,
) -> Result<CallToolResult, Error> {
    let params = CallToolParams {
        name: name.to_string(),
        arguments,
        input_responses: None,
        request_state: None,
        meta: None,
        task: None,
    };
    let response = router
        .call(tower_mcp::RouterRequest::new(
            RequestId::Number(2),
            McpRequest::CallTool(params),
        ))
        .await
        .expect("router error type is Infallible");
    match response.inner {
        Ok(McpResponse::CallTool(result)) => Ok(result),
        Ok(other) => Err(format!("unexpected tools/call response: {other:?}").into()),
        Err(error) => Err(format!("{name}: {}", error.message).into()),
    }
}

/// Text content to stdout, error text to stderr; returns whether the
/// result was an error.
fn print_result(result: &CallToolResult) -> bool {
    for content in &result.content {
        if let Content::Text { text, .. } = content {
            if result.is_error {
                eprintln!("{text}");
            } else {
                println!("{text}");
            }
        }
    }
    result.is_error
}

/// One subcommand per tool, one flag per schema property. Everything is
/// a flag rather than a positional: property order in the schema map is
/// not authored order, so positional derivation would guess.
fn derive_command(tools: &[ToolDefinition]) -> Command {
    let mut command = Command::new("ciacola")
        .about("Durable agent server; with a subcommand, a one-shot operator call against the same ledger")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(Command::new("serve").about("Run the server (the default when no arguments are given)"));
    for tool in tools {
        command = command.subcommand(subcommand_for(tool));
    }
    command
}

fn subcommand_for(tool: &ToolDefinition) -> Command {
    let mut subcommand = Command::new(tool.name.replace('_', "-"));
    if let Some(description) = &tool.description {
        let first_line = description.lines().next().unwrap_or_default().to_string();
        subcommand = subcommand.about(first_line).long_about(description.clone());
    }
    let required = required_set(&tool.input_schema);
    if let Some(properties) = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    {
        for (name, schema) in properties {
            subcommand = subcommand.arg(argument_for(
                name,
                schema,
                required.contains(&name.as_str()),
            ));
        }
    }
    if tool.name == "send" {
        subcommand = subcommand
            .arg(
                Arg::new("wait")
                    .long("wait")
                    .action(ArgAction::SetTrue)
                    .help("After queueing, wait for a running server to settle the turn"),
            )
            .arg(
                Arg::new("wait-timeout-secs")
                    .long("wait-timeout-secs")
                    .value_parser(clap::value_parser!(i64))
                    .help("Give up waiting after this long (server default applies when omitted)"),
            );
    }
    subcommand
}

fn required_set(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn argument_for(name: &str, schema: &Value, required: bool) -> Arg {
    let mut argument = Arg::new(name.to_string()).long(name.replace('_', "-"));
    if let Some(help) = schema.get("description").and_then(Value::as_str) {
        argument = argument.help(help.to_string());
    }
    let kind = schema_type(schema);
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        let values: Vec<String> = choices
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        argument = argument.value_parser(PossibleValuesParser::new(values));
    } else {
        argument = match kind {
            "boolean" => argument.action(ArgAction::SetTrue),
            "integer" => argument
                .value_parser(clap::value_parser!(i64))
                .allow_hyphen_values(true),
            "number" => argument
                .value_parser(clap::value_parser!(f64))
                .allow_hyphen_values(true),
            "array" => argument.action(ArgAction::Append),
            "object" => argument.action(ArgAction::Append).value_name("KEY=VALUE"),
            _ => argument,
        };
    }
    if required && kind != "boolean" {
        argument = argument.required(true);
    }
    argument
}

/// The declared type, looking through nullable unions like
/// `["string", "null"]`.
fn schema_type(schema: &Value) -> &str {
    match schema.get("type") {
        Some(Value::String(kind)) => kind,
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null")
            .unwrap_or("string"),
        _ => "string",
    }
}

fn arguments_from_matches(tool: &ToolDefinition, matches: &ArgMatches) -> Result<Value, Error> {
    let mut arguments = Map::new();
    let Some(properties) = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    else {
        return Ok(Value::Object(arguments));
    };
    for (name, schema) in properties {
        let id = name.as_str();
        if schema.get("enum").is_some() {
            if let Some(value) = matches.get_one::<String>(id) {
                arguments.insert(name.clone(), json!(value));
            }
            continue;
        }
        match schema_type(schema) {
            "boolean" => {
                if matches.get_flag(id) {
                    arguments.insert(name.clone(), json!(true));
                }
            }
            "integer" => {
                if let Some(value) = matches.get_one::<i64>(id) {
                    arguments.insert(name.clone(), json!(value));
                }
            }
            "number" => {
                if let Some(value) = matches.get_one::<f64>(id) {
                    arguments.insert(name.clone(), json!(value));
                }
            }
            "array" => {
                if let Some(values) = matches.get_many::<String>(id) {
                    let item_kind = schema.get("items").map(schema_type).unwrap_or("string");
                    let items: Result<Vec<Value>, Error> =
                        values.map(|value| scalar_from(value, item_kind)).collect();
                    arguments.insert(name.clone(), Value::Array(items?));
                }
            }
            "object" => {
                if let Some(values) = matches.get_many::<String>(id) {
                    let mut object = Map::new();
                    for pair in values {
                        let Some((key, value)) = pair.split_once('=') else {
                            return Err(format!(
                                "--{} takes KEY=VALUE, got '{pair}'",
                                name.replace('_', "-")
                            )
                            .into());
                        };
                        object.insert(key.to_string(), untyped_scalar(value));
                    }
                    arguments.insert(name.clone(), Value::Object(object));
                }
            }
            _ => {
                if let Some(value) = matches.get_one::<String>(id) {
                    arguments.insert(name.clone(), json!(value));
                }
            }
        }
    }
    Ok(Value::Object(arguments))
}

fn scalar_from(value: &str, kind: &str) -> Result<Value, Error> {
    match kind {
        "integer" => Ok(json!(value.parse::<i64>()?)),
        "number" => Ok(json!(value.parse::<f64>()?)),
        "boolean" => Ok(json!(value.parse::<bool>()?)),
        _ => Ok(json!(value)),
    }
}

/// For object values the schema does not type: numbers and booleans
/// parse as themselves, everything else stays a string.
fn untyped_scalar(value: &str) -> Value {
    if let Ok(number) = value.parse::<i64>() {
        return json!(number);
    }
    if let Ok(number) = value.parse::<f64>() {
        return json!(number);
    }
    if let Ok(flag) = value.parse::<bool>() {
        return json!(flag);
    }
    json!(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, schema: Value) -> ToolDefinition {
        serde_json::from_value(json!({
            "name": name,
            "description": "First line.\nRest of it.",
            "inputSchema": schema,
        }))
        .expect("tool definition")
    }

    #[test]
    fn flags_derive_per_property_with_required_and_enums() {
        let tools = vec![tool(
            "track",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Item title"},
                    "lane": {"type": "string", "enum": ["todo", "doing", "done", "dropped"]},
                    "priority": {"type": "integer"},
                },
                "required": ["title", "lane"],
            }),
        )];
        let command = derive_command(&tools);
        let matches = command
            .try_get_matches_from([
                "ciacola",
                "track",
                "--title",
                "a thing",
                "--lane",
                "doing",
                "--priority",
                "2",
            ])
            .expect("parse");
        let (name, sub) = matches.subcommand().expect("subcommand");
        assert_eq!(name, "track");
        let arguments = arguments_from_matches(&tools[0], sub).expect("arguments");
        assert_eq!(
            arguments,
            json!({"title": "a thing", "lane": "doing", "priority": 2})
        );
    }

    #[test]
    fn missing_required_and_bad_enum_values_are_parse_errors() {
        let tools = vec![tool(
            "track",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "lane": {"type": "string", "enum": ["todo", "doing"]},
                },
                "required": ["title", "lane"],
            }),
        )];
        assert!(
            derive_command(&tools)
                .try_get_matches_from(["ciacola", "track", "--lane", "todo"])
                .is_err()
        );
        let tools = vec![tool(
            "track",
            json!({
                "type": "object",
                "properties": {
                    "lane": {"type": "string", "enum": ["todo", "doing"]},
                },
                "required": [],
            }),
        )];
        assert!(
            derive_command(&tools)
                .try_get_matches_from(["ciacola", "track", "--lane", "sideways"])
                .is_err()
        );
    }

    #[test]
    fn arrays_append_and_objects_take_key_value_pairs() {
        let tools = vec![
            tool(
                "save_ref",
                json!({
                    "type": "object",
                    "properties": {
                        "tags": {"type": "array", "items": {"type": "string"}},
                    },
                }),
            ),
            tool(
                "spawn_role",
                json!({
                    "type": "object",
                    "properties": {
                        "arguments": {"type": "object"},
                    },
                }),
            ),
        ];
        let command = derive_command(&tools);
        let matches = command
            .clone()
            .try_get_matches_from(["ciacola", "save-ref", "--tags", "a", "--tags", "b"])
            .expect("parse");
        let arguments =
            arguments_from_matches(&tools[0], matches.subcommand().unwrap().1).expect("arguments");
        assert_eq!(arguments, json!({"tags": ["a", "b"]}));

        let matches = command
            .try_get_matches_from([
                "ciacola",
                "spawn-role",
                "--arguments",
                "repo=owner/name",
                "--arguments",
                "issue=5",
            ])
            .expect("parse");
        let arguments =
            arguments_from_matches(&tools[1], matches.subcommand().unwrap().1).expect("arguments");
        assert_eq!(
            arguments,
            json!({"arguments": {"repo": "owner/name", "issue": 5}})
        );
    }

    #[test]
    fn booleans_are_presence_flags_and_absent_means_server_default() {
        let tools = vec![tool(
            "open_pr",
            json!({
                "type": "object",
                "properties": {
                    "draft": {"type": "boolean"},
                    "title": {"type": "string"},
                },
                "required": ["title"],
            }),
        )];
        let command = derive_command(&tools);
        let matches = command
            .try_get_matches_from(["ciacola", "open-pr", "--title", "fix: x", "--draft"])
            .expect("parse");
        let arguments =
            arguments_from_matches(&tools[0], matches.subcommand().unwrap().1).expect("arguments");
        assert_eq!(arguments, json!({"title": "fix: x", "draft": true}));

        let command = derive_command(&tools);
        let matches = command
            .try_get_matches_from(["ciacola", "open-pr", "--title", "fix: x"])
            .expect("parse");
        let arguments =
            arguments_from_matches(&tools[0], matches.subcommand().unwrap().1).expect("arguments");
        assert_eq!(arguments, json!({"title": "fix: x"}));
    }
}
