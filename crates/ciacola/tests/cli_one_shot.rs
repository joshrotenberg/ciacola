//! The one-shot CLI is the same operator surface as the server: a
//! subcommand per tool, derived from `tools/list`, driving the shared
//! ledger and exiting. These tests run the real binary.

use std::path::Path;
use std::process::{Command, Output};

fn test_directory(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "ciacola-cli-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir(&directory).expect("test directory");
    std::fs::write(directory.join("ciacola.toml"), "").expect("empty config");
    directory
}

fn ciacola(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ciacola"))
        .args(args)
        .current_dir(directory)
        .env("CIACOLA_CONFIG", directory.join("ciacola.toml"))
        .env("CIACOLA_DB", directory.join("ciacola.db"))
        .env("CIACOLA_HTTP", "0")
        .env("TMPDIR", directory)
        .env_remove("CIACOLA_OPERATOR_TOKEN")
        .env_remove("CIACOLA_OPERATOR_TOKEN_FD")
        .env_remove("MCP_BEARER")
        .output()
        .expect("run ciacola")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn spawn_get_and_send_drive_the_shared_ledger_and_send_stays_queued() {
    let directory = test_directory("lifecycle");

    // spawn: defines the agent, runs nothing.
    let spawned = ciacola(
        &directory,
        &[
            "spawn",
            "--name",
            "cli-proof",
            "--system-prompt",
            "You exist to prove the CLI works.",
        ],
    );
    assert!(
        spawned.status.success(),
        "spawn failed: {}",
        String::from_utf8_lossy(&spawned.stderr)
    );
    let reply: serde_json::Value =
        serde_json::from_str(stdout(&spawned).trim()).expect("spawn returns JSON");
    let agent_id = reply["agent_id"].as_str().expect("agent_id").to_string();

    // A second invocation sees the same durable agent.
    let listed = ciacola(&directory, &["list"]);
    assert!(listed.status.success());
    assert!(
        stdout(&listed).contains(&agent_id),
        "list must show the spawned agent: {}",
        stdout(&listed)
    );

    // send queues and returns a seq; one-shot never executes, so the
    // turn must still be queued afterwards (dispatch stays closed and
    // no provider exists here to run it anyway).
    let sent = ciacola(
        &directory,
        &["send", "--agent-id", &agent_id, "--text", "hello"],
    );
    assert!(
        sent.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let reply: serde_json::Value =
        serde_json::from_str(stdout(&sent).trim()).expect("send returns JSON");
    assert_eq!(reply["seq"].as_i64(), Some(1));

    let got = ciacola(&directory, &["get", "--agent-id", &agent_id]);
    assert!(got.status.success());
    assert!(
        stdout(&got).contains("queued"),
        "the sent turn must still be queued, not executed: {}",
        stdout(&got)
    );
}

#[test]
fn unknown_tools_and_missing_required_arguments_fail_with_usage() {
    let directory = test_directory("errors");

    let unknown = ciacola(&directory, &["no-such-tool"]);
    assert!(!unknown.status.success());

    // spawn requires --name and --system-prompt; clap reports the gap
    // before any tool call happens.
    let missing = ciacola(&directory, &["spawn", "--name", "only-a-name"]);
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("--system-prompt"),
        "usage error should name the missing flag: {stderr}"
    );
}

#[test]
fn help_lists_the_derived_subcommands() {
    let directory = test_directory("help");
    let help = ciacola(&directory, &["--help"]);
    assert!(help.status.success(), "--help should exit 0");
    let text = stdout(&help);
    for expected in ["serve", "spawn", "send", "wait", "list", "kill"] {
        assert!(text.contains(expected), "help must list {expected}: {text}");
    }
}
