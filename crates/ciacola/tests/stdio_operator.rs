//! The operator credential belongs only to HTTP. The stdio product surface
//! must remain directly usable by a human MCP client with no secret setup.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn response_with_id(receiver: &mpsc::Receiver<String>, id: i64) -> serde_json::Value {
    loop {
        let line = receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|error| panic!("stdio response {id} timed out: {error}"));
        let value: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("invalid JSON {line:?}: {error}"));
        if value.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
            return value;
        }
    }
}

#[test]
fn stdio_stays_an_unauthenticated_interactive_operator_surface() {
    let directory = std::env::temp_dir().join(format!(
        "ciacola-stdio-auth-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir(&directory).expect("test directory");
    let config = directory.join("ciacola.toml");
    std::fs::write(&config, "").expect("empty config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_ciacola"));
    command
        .current_dir(&directory)
        .env("CIACOLA_CONFIG", &config)
        .env("CIACOLA_DB", directory.join("ciacola.db"))
        .env("CIACOLA_HTTP", "0")
        .env("CIACOLA_NO_RECOVER", "1")
        .env("TMPDIR", &directory)
        .env_remove("CIACOLA_OPERATOR_TOKEN")
        .env_remove("CIACOLA_OPERATOR_TOKEN_FD")
        .env_remove("MCP_BEARER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"stdio-auth-test","version":"1"}}}}}}"#
    )
    .expect("initialize");
    stdin.flush().expect("flush initialize");
    let initialize = response_with_id(&receiver, 1);
    assert!(initialize.get("result").is_some(), "{initialize}");

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .expect("initialized notification");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
    )
    .expect("list tools");
    stdin.flush().expect("flush tools/list");

    let tools = response_with_id(&receiver, 2);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"kill"),
        "stdio lost operator tool: {names:?}"
    );
    assert!(
        names.contains(&"send_supervised"),
        "stdio lost interactive admission: {names:?}"
    );

    drop(child);
    std::fs::remove_dir_all(directory).expect("cleanup");
}
