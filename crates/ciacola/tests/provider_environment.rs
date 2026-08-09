//! Whole-binary regression for the provider-child startup authority path.
//!
//! Unit tests pin each seam independently. This test carries a controlled
//! ambient environment and an inherited credential descriptor through real
//! config loading, pre-Tokio ingestion, provider construction, opening, and
//! resume, then inspects the complete fake Codex child environments.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CREDENTIAL_FD: i32 = 9;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn response_with_id(
    receiver: &mpsc::Receiver<String>,
    id: i64,
    stderr_path: &std::path::Path,
) -> serde_json::Value {
    loop {
        let line = receiver
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|error| {
                let stderr = std::fs::read_to_string(stderr_path)
                    .unwrap_or_else(|read_error| format!("<unreadable: {read_error}>"));
                panic!("stdio response {id} timed out: {error}\nchild stderr:\n{stderr}")
            });
        let value: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("invalid JSON {line:?}: {error}"));
        if value.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
            return value;
        }
    }
}

fn send_request(stdin: &mut impl Write, id: i64, method: &str, params: serde_json::Value) {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(stdin, "{request}").expect("write JSON-RPC request");
    stdin.flush().expect("flush JSON-RPC request");
}

fn call_tool(
    stdin: &mut impl Write,
    receiver: &mpsc::Receiver<String>,
    stderr_path: &std::path::Path,
    id: i64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    send_request(
        stdin,
        id,
        "tools/call",
        serde_json::json!({ "name": name, "arguments": arguments }),
    );
    let response = response_with_id(receiver, id, stderr_path);
    assert!(
        response.get("error").is_none(),
        "tool {name} failed: {response}"
    );
    response
}

fn captured_environment(path: &std::path::Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        .lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(name, value)| (name.to_string(), value.to_string()))
        })
        .collect()
}

fn environment_keys(environment: &BTreeMap<String, String>) -> Vec<&str> {
    environment.keys().map(String::as_str).collect()
}

#[test]
fn startup_ingestion_builds_clean_open_and_resume_children() {
    let directory = tempfile::tempdir().expect("temporary startup fixture");
    let root = directory.path();
    let bin = root.join("bin");
    let capture = root.join("capture");
    std::fs::create_dir(&bin).expect("fake binary directory");
    std::fs::create_dir(&capture).expect("capture directory");

    let fake_codex = bin.join("codex");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fake-codex-startup-environment.sh"),
        &fake_codex,
    )
    .expect("copy fake codex");
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o700))
        .expect("make fake codex executable");

    let config = root.join("ciacola.toml");
    let stderr_path = root.join("ciacola.stderr");
    std::fs::write(
        &config,
        r#"
            [runtime]
            default_provider = "codex"
            provider_env_passthrough = [
                "TEST_CAPTURE_DIR",
                "MCP_BEARER",
                "OPENAI_API_KEY",
                "CODEX_API_KEY",
                "CODEX_ACCESS_TOKEN",
                "CIACOLA_OPERATOR_TOKEN_FD",
                "CIACOLA_CLAUDE_TOKEN_FD",
                "CIACOLA_CODEX_TOKEN_FD",
            ]
        "#,
    )
    .expect("startup config");

    let mut credential = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(root.join("codex.credential"))
        .expect("credential source");
    credential
        .write_all(b"intended-codex-descriptor-credential\n")
        .expect("write credential");
    credential.rewind().expect("rewind credential");
    let credential_source = credential.as_raw_fd();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ciacola"));
    command
        .current_dir(root)
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", root.join("home"))
        .env("LANG", "C")
        .env("SHELL", "/bin/bash")
        .env("TMPDIR", root)
        .env("CIACOLA_CONFIG", &config)
        .env("CIACOLA_DB", root.join("ciacola.db"))
        .env("CIACOLA_HTTP", "0")
        .env("CIACOLA_NO_RECOVER", "1")
        .env("CIACOLA_CODEX_TOKEN_FD", CREDENTIAL_FD.to_string())
        .env("TEST_CAPTURE_DIR", &capture)
        .env("MCP_BEARER", "deliberate-client-bearer")
        .env("OPENAI_API_KEY", "wrong-openai-key")
        .env("CODEX_API_KEY", "wrong-codex-key")
        .env("CODEX_ACCESS_TOKEN", "wrong-codex-token")
        .env("ANTHROPIC_API_KEY", "ambient-opposite-provider-key")
        .env("CIACOLA_ISSUE80_SENTINEL", "ambient-ciacola-value")
        .env("UNRELATED_SECRET", "ambient-unrelated-value")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("child stderr capture"),
        ));
    // SAFETY: the child-side closure performs only the async-signal-safe dup2
    // syscall. The source remains owned by the parent; dup2 clears CLOEXEC on
    // the fixed child descriptor that main consumes and closes before Tokio.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(credential_source, CREDENTIAL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = ChildGuard(command.spawn().expect("start ciacola"));
    let stdout = child.0.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let stdin = child.0.stdin.as_mut().expect("stdin");

    send_request(
        stdin,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "provider-env-test", "version": "1" },
        }),
    );
    let initialize = response_with_id(&receiver, 1, &stderr_path);
    assert!(initialize.get("result").is_some(), "{initialize}");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .expect("initialized notification");
    stdin.flush().expect("flush initialized notification");

    let spawned = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        2,
        "spawn",
        serde_json::json!({
            "name": "startup-environment",
            "system_prompt": "Exercise opening and resume.",
            "provider": "codex",
            "inherit_provider_tools": true,
            "sandbox": "workspace-write-no-network",
        }),
    );
    let agent_id = spawned["result"]["structuredContent"]["agent_id"]
        .as_str()
        .expect("spawned agent id")
        .to_string();

    let opening = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        3,
        "send_supervised",
        serde_json::json!({
            "agent_id": agent_id,
            "text": "opening",
            "reason": "whole-binary provider environment regression",
        }),
    );
    assert_eq!(opening["result"]["structuredContent"]["seq"], 1);
    let opened = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        4,
        "wait",
        serde_json::json!({ "agent_id": agent_id, "seq": 1, "timeout_secs": 20 }),
    );
    assert_eq!(opened["result"]["structuredContent"]["state"], "ok");

    let resume = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        5,
        "send_supervised",
        serde_json::json!({
            "agent_id": agent_id,
            "text": "resume",
            "reason": "whole-binary provider environment regression",
        }),
    );
    assert_eq!(resume["result"]["structuredContent"]["seq"], 2);
    let resumed = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        6,
        "wait",
        serde_json::json!({ "agent_id": agent_id, "seq": 2, "timeout_secs": 20 }),
    );
    assert_eq!(resumed["result"]["structuredContent"]["state"], "ok");

    let version_env = captured_environment(&capture.join("version.env"));
    let open_env = captured_environment(&capture.join("open.env"));
    let resume_env = captured_environment(&capture.join("resume.env"));

    for (label, environment) in [
        ("version", &version_env),
        ("open", &open_env),
        ("resume", &resume_env),
    ] {
        assert_eq!(
            environment.get("MCP_BEARER").map(String::as_str),
            Some("deliberate-client-bearer")
        );
        for absent in [
            "CIACOLA_OPERATOR_TOKEN_FD",
            "CIACOLA_CLAUDE_TOKEN_FD",
            "CIACOLA_CODEX_TOKEN_FD",
            "OPENAI_API_KEY",
            "CODEX_ACCESS_TOKEN",
            "ANTHROPIC_API_KEY",
            "CIACOLA_ISSUE80_SENTINEL",
            "UNRELATED_SECRET",
        ] {
            assert!(
                !environment.contains_key(absent),
                "{label} child unexpectedly received {absent}; exported keys: {:?}",
                environment_keys(environment)
            );
        }
    }
    assert!(
        !version_env.contains_key("CODEX_API_KEY"),
        "the credential is unnecessary for the version probe"
    );
    for environment in [&open_env, &resume_env] {
        assert_eq!(
            environment.get("CODEX_API_KEY").map(String::as_str),
            Some("intended-codex-descriptor-credential")
        );
        assert_eq!(
            environment.get("TEST_CAPTURE_DIR").map(String::as_str),
            Some(capture.to_string_lossy().as_ref())
        );
    }
    assert_eq!(
        environment_keys(&open_env),
        environment_keys(&resume_env),
        "opening and resume exported different environment names"
    );
    assert_eq!(
        std::fs::read_to_string(capture.join("open.fd9")).expect("open fd state"),
        "closed\n"
    );
    assert_eq!(
        std::fs::read_to_string(capture.join("resume.fd9")).expect("resume fd state"),
        "closed\n"
    );

    let credential_bytes = b"intended-codex-descriptor-credential";
    for path in [
        root.join("ciacola.db"),
        root.join("ciacola.db-wal"),
        root.join("ciacola.db-shm"),
    ] {
        if path.exists() {
            let bytes = std::fs::read(&path).expect("read ledger artifact");
            assert!(
                !bytes
                    .windows(credential_bytes.len())
                    .any(|window| window == credential_bytes),
                "provider credential reached ledger artifact {}",
                path.display()
            );
        }
    }
}
