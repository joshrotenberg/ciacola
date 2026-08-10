//! Whole-binary regression for the Ctrl-C contract in README "Operating
//! Ciacola": the first SIGINT stops HTTP intake and drains in-flight turns,
//! a second SIGINT abandons them for restart recovery, and both paths leave
//! a reopenable ledger behind.
//!
//! Determinism notes. Readiness is a bounded poll of the unauthenticated
//! `/board` endpoint, never a sleep. A served stdio response proves the
//! shutdown `select!` is being polled, so the SIGINT handler is installed
//! before the first signal is sent. The in-flight turn is a fake provider
//! CLI that announces itself through a marker file and then blocks, so no
//! live provider and no timing assumption is involved.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// The abandoned provider child deliberately outlives the server. Reap it
/// on every test exit path so a failing assertion cannot leak a ten-minute
/// sleeper into CI.
struct FakeProviderReaper(PathBuf);

impl Drop for FakeProviderReaper {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|content| content.trim().parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 0)
        {
            // SAFETY: plain kill(2) on a specific positive pid read from the
            // fixture's marker file; no memory or handles are involved.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn reserve_loopback_port() -> u16 {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("reserve a loopback port")
        .local_addr()
        .expect("reserved port address")
        .port()
}

fn stdout_lines(child: &mut Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn child_stderr(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| format!("<unreadable: {error}>"))
}

fn response_with_id(
    receiver: &mpsc::Receiver<String>,
    id: i64,
    stderr_path: &Path,
) -> serde_json::Value {
    loop {
        let line = receiver
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|error| {
                panic!(
                    "stdio response {id} timed out: {error}\nchild stderr:\n{}",
                    child_stderr(stderr_path)
                )
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

fn initialize_stdio(stdin: &mut impl Write, receiver: &mpsc::Receiver<String>, stderr_path: &Path) {
    send_request(
        stdin,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "graceful-shutdown-test", "version": "1" },
        }),
    );
    let initialize = response_with_id(receiver, 1, stderr_path);
    assert!(initialize.get("result").is_some(), "{initialize}");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .expect("initialized notification");
    stdin.flush().expect("flush initialized notification");
}

fn call_tool(
    stdin: &mut impl Write,
    receiver: &mpsc::Receiver<String>,
    stderr_path: &Path,
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

fn board_responds(port: u16) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(500)) else {
        return false;
    };
    let io_budget = Some(Duration::from_secs(5));
    if stream.set_read_timeout(io_budget).is_err() || stream.set_write_timeout(io_budget).is_err() {
        return false;
    }
    if stream
        .write_all(b"GET /board HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    if stream.read_to_end(&mut response).is_err() {
        return false;
    }
    response.starts_with(b"HTTP/1.1 200")
}

fn port_refuses_connections(port: u16) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_err()
}

fn wait_until(
    what: &str,
    stderr_path: &Path,
    timeout: Duration,
    mut condition: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "timed out waiting for {what}\nchild stderr:\n{}",
        child_stderr(stderr_path)
    );
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll ciacola for exit") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn send_sigint(child: &Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: plain kill(2) addressed to one known child pid; no memory or
    // handles are involved.
    let result = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(
        result,
        0,
        "kill -INT {pid}: {}",
        std::io::Error::last_os_error()
    );
}

fn stderr_contains(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|content| content.contains(needle))
}

/// Reopen the ledger on a fresh connection and return the schema_migrations
/// row count plus every turn as (agent_id, seq, state).
fn ledger_snapshot(path: &Path) -> (i64, Vec<(String, i64, String)>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("ledger inspection runtime");
    runtime.block_on(async {
        use sqlx::Connection;
        let mut connection =
            sqlx::SqliteConnection::connect(&format!("sqlite://{}?mode=rw", path.display()))
                .await
                .expect("reopen the ledger after shutdown");
        let (migrations,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&mut connection)
            .await
            .expect("count schema_migrations");
        let turns: Vec<(String, i64, String)> =
            sqlx::query_as("SELECT agent_id, seq, state FROM turns ORDER BY agent_id, seq")
                .fetch_all(&mut connection)
                .await
                .expect("read turns");
        (migrations, turns)
    })
}

#[test]
fn first_sigint_drains_empty_and_exits_cleanly() {
    let directory = tempfile::tempdir().expect("temporary shutdown fixture");
    let root = directory.path();
    let config = root.join("ciacola.toml");
    std::fs::write(&config, "").expect("empty config");
    let database = root.join("ciacola.db");
    let stderr_path = root.join("ciacola.stderr");
    let port = reserve_loopback_port();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ciacola"));
    command
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("LANG", "C")
        .env("SHELL", "/bin/bash")
        .env("TMPDIR", root)
        .env("CIACOLA_CONFIG", &config)
        .env("CIACOLA_DB", &database)
        .env("CIACOLA_HTTP", port.to_string())
        .env("CIACOLA_NO_RECOVER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("child stderr capture"),
        ));
    let mut child = ChildGuard(command.spawn().expect("start ciacola"));
    let receiver = stdout_lines(&mut child.0);
    let stdin = child.0.stdin.as_mut().expect("stdin");

    wait_until(
        "the board endpoint to serve",
        &stderr_path,
        Duration::from_secs(30),
        || board_responds(port),
    );
    initialize_stdio(stdin, &receiver, &stderr_path);

    send_sigint(&child.0);
    let status = wait_for_exit(&mut child.0, Duration::from_secs(60)).unwrap_or_else(|| {
        panic!(
            "SIGINT with nothing in flight did not end the process\nchild stderr:\n{}",
            child_stderr(&stderr_path)
        )
    });
    assert!(
        status.success(),
        "expected a clean drain exit, got {status}"
    );
    assert!(
        stderr_contains(&stderr_path, "draining, in-flight turns finish"),
        "missing drain announcement\nchild stderr:\n{}",
        child_stderr(&stderr_path)
    );
    assert!(
        stderr_contains(&stderr_path, "drained clean"),
        "missing clean drain confirmation\nchild stderr:\n{}",
        child_stderr(&stderr_path)
    );
    assert!(
        port_refuses_connections(port),
        "the drained server left the HTTP port accepting"
    );

    let (migrations, turns) = ledger_snapshot(&database);
    assert!(migrations > 0, "schema_migrations lost its rows");
    assert!(turns.is_empty(), "an empty drain invented turns: {turns:?}");
}

#[test]
fn second_sigint_abandons_an_in_flight_turn_for_recovery() {
    let directory = tempfile::tempdir().expect("temporary shutdown fixture");
    let root = directory.path();
    let bin = root.join("bin");
    let marker = root.join("marker");
    std::fs::create_dir(&bin).expect("fake binary directory");
    std::fs::create_dir(&marker).expect("marker directory");
    let reaper = FakeProviderReaper(marker.join("turn.pid"));

    let fake_codex = bin.join("codex");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-codex-hanging-turn.sh"),
        &fake_codex,
    )
    .expect("copy fake codex");
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o700))
        .expect("make fake codex executable");

    let config = root.join("ciacola.toml");
    std::fs::write(
        &config,
        r#"
            [runtime]
            default_provider = "codex"
            provider_env_passthrough = ["TEST_MARKER_DIR"]
        "#,
    )
    .expect("shutdown config");
    let database = root.join("ciacola.db");
    let stderr_path = root.join("ciacola.stderr");
    let port = reserve_loopback_port();

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
        .env("CIACOLA_DB", &database)
        .env("CIACOLA_HTTP", port.to_string())
        .env("CIACOLA_NO_RECOVER", "1")
        .env("TEST_MARKER_DIR", &marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("child stderr capture"),
        ));
    let mut child = ChildGuard(command.spawn().expect("start ciacola"));
    let receiver = stdout_lines(&mut child.0);
    let stdin = child.0.stdin.as_mut().expect("stdin");

    wait_until(
        "the board endpoint to serve",
        &stderr_path,
        Duration::from_secs(30),
        || board_responds(port),
    );
    initialize_stdio(stdin, &receiver, &stderr_path);

    let spawned = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        2,
        "spawn",
        serde_json::json!({
            "name": "graceful-shutdown",
            "system_prompt": "Hold one turn in flight.",
            "provider": "codex",
            "inherit_provider_tools": true,
            "sandbox": "workspace-write-no-network",
        }),
    );
    let agent_id = spawned["result"]["structuredContent"]["agent_id"]
        .as_str()
        .expect("spawned agent id")
        .to_string();
    let submitted = call_tool(
        stdin,
        &receiver,
        &stderr_path,
        3,
        "send_supervised",
        serde_json::json!({
            "agent_id": agent_id,
            "text": "hold this turn open",
            "reason": "whole-binary graceful shutdown regression",
        }),
    );
    assert_eq!(submitted["result"]["structuredContent"]["seq"], 1);

    // The fake provider writes its pid once the turn is genuinely running.
    wait_until(
        "the fake provider to hold a turn in flight",
        &stderr_path,
        Duration::from_secs(30),
        || reaper.0.exists(),
    );

    // First SIGINT: intake stops, the in-flight turn keeps the drain open.
    send_sigint(&child.0);
    wait_until(
        "the drain announcement",
        &stderr_path,
        Duration::from_secs(30),
        || stderr_contains(&stderr_path, "draining, in-flight turns finish"),
    );
    wait_until(
        "HTTP intake to stop while draining",
        &stderr_path,
        Duration::from_secs(30),
        || port_refuses_connections(port),
    );
    assert!(
        child.0.try_wait().expect("poll ciacola").is_none(),
        "the server exited while its turn was still in flight\nchild stderr:\n{}",
        child_stderr(&stderr_path)
    );

    // Second SIGINT: abandon the turn for restart recovery. Retried on a
    // bounded loop because a signal landing in the instant between the drain
    // announcement and the inner listener registration would be dropped.
    let mut status = None;
    for _ in 0..15 {
        send_sigint(&child.0);
        if let Some(exit) = wait_for_exit(&mut child.0, Duration::from_secs(2)) {
            status = Some(exit);
            break;
        }
    }
    let status = status.unwrap_or_else(|| {
        panic!(
            "a second SIGINT did not abandon the drain\nchild stderr:\n{}",
            child_stderr(&stderr_path)
        )
    });
    assert!(
        status.success(),
        "expected a clean abandon exit, got {status}"
    );
    assert!(
        stderr_contains(&stderr_path, "abandoning in-flight turns"),
        "missing abandon announcement\nchild stderr:\n{}",
        child_stderr(&stderr_path)
    );
    assert!(
        port_refuses_connections(port),
        "the abandoned server left the HTTP port accepting"
    );

    let (migrations, turns) = ledger_snapshot(&database);
    assert!(migrations > 0, "schema_migrations lost its rows");
    assert_eq!(
        turns.len(),
        1,
        "expected exactly the abandoned turn: {turns:?}"
    );
    assert_eq!(turns[0].1, 1, "unexpected turn seq: {turns:?}");
    assert_eq!(
        turns[0].2, "running",
        "the abandoned turn must stay parked for restart recovery: {turns:?}"
    );
    drop(reaper);
}
