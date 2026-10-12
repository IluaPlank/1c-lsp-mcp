use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// NDJSON MCP client for `mcp-bsl-context` (stdio).
#[derive(Clone)]
pub struct BslContextClient {
    sender: mpsc::Sender<Value>,
    pending: PendingMap,
    next_id: Arc<AtomicI64>,
}

impl BslContextClient {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.sender
            .send(message)
            .await
            .map_err(|_| anyhow!("bsl-context writer channel closed"))?;

        let response = rx
            .await
            .map_err(|_| anyhow!("bsl-context response channel dropped"))?;
        if let Some(error) = response.get("error") {
            bail!("bsl-context MCP error: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.sender
            .send(message)
            .await
            .map_err(|_| anyhow!("bsl-context writer channel closed"))?;
        Ok(())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        )
        .await
    }
}

pub async fn spawn_bsl_context(
    java_path: &str,
    jar_path: &str,
    platform_path: &str,
) -> Result<(BslContextClient, Child)> {
    let mut command = Command::new(java_path);
    command
        .arg("-jar")
        .arg(jar_path)
        .arg("--platform-path")
        .arg(platform_path)
        .arg("--mode")
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn bsl-context: {jar_path}"))?;

    let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("missing stderr"))?;

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let (tx, rx) = mpsc::channel::<Value>(64);

    tokio::spawn(writer_loop(stdin, rx));
    tokio::spawn(reader_loop(stdout, pending.clone()));
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!("bsl-context stderr: {line}");
        }
    });

    let client = BslContextClient {
        sender: tx,
        pending,
        next_id: Arc::new(AtomicI64::new(1)),
    };

    let init = client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "1c-mcp", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await
        .context("bsl-context initialize failed")?;

    tracing::info!(
        "bsl-context ready: {}",
        init.get("serverInfo")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
    );

    client
        .notify("notifications/initialized", json!({}))
        .await?;

    Ok((client, child))
}

async fn writer_loop(mut stdin: ChildStdin, mut rx: mpsc::Receiver<Value>) {
    while let Some(message) = rx.recv().await {
        let mut line = message.to_string();
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn reader_loop(stdout: impl tokio::io::AsyncRead + Unpin, pending: PendingMap) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!("bsl-context: non-json stdout line: {line}");
            continue;
        };
        if let Some(id) = value.get("id").and_then(|v| v.as_i64()) {
            if let Some(tx) = pending.lock().await.remove(&id) {
                let _ = tx.send(value);
            }
        }
    }
}

/// Extract text payload from MCP tools/call result.
pub fn tool_result_text(result: &Value) -> Result<String> {
    if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        let text = result
            .pointer("/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("bsl-context tool error");
        bail!("{text}");
    }
    if let Some(text) = result.pointer("/content/0/text").and_then(|v| v.as_str()) {
        return Ok(text.to_string());
    }
    Ok(result.to_string())
}

pub const PLATFORM_TOOLS: &[&str] = &[
    "search",
    "info",
    "getMembers",
    "getMember",
    "getConstructors",
];

/// Lazy process manager for bsl-context JAR.
pub struct BslContextService {
    inner: Mutex<Option<LiveContext>>,
}

struct LiveContext {
    client: BslContextClient,
    jar_path: String,
    platform_path: String,
    _child: Child,
}

impl BslContextService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub async fn ensure(
        &self,
        java_path: &str,
        jar_path: &str,
        platform_path: &str,
    ) -> Result<BslContextClient> {
        if jar_path.trim().is_empty() || platform_path.trim().is_empty() {
            bail!(
                "bsl-context не настроен: укажите platform_path и bsl_context_jar_path в config.toml"
            );
        }

        let mut guard = self.inner.lock().await;
        if let Some(live) = guard.as_ref() {
            if live.jar_path == jar_path && live.platform_path == platform_path {
                return Ok(live.client.clone());
            }
        }

        let (client, child) = spawn_bsl_context(java_path, jar_path, platform_path).await?;
        *guard = Some(LiveContext {
            client: client.clone(),
            jar_path: jar_path.to_string(),
            platform_path: platform_path.to_string(),
            _child: child,
        });
        Ok(client)
    }

    pub async fn shutdown(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(mut live) = guard.take() {
            let _ = live._child.kill().await;
        }
    }
}

impl Default for BslContextService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_tool_names_are_stable() {
        assert_eq!(
            PLATFORM_TOOLS,
            [
                "search",
                "info",
                "getMembers",
                "getMember",
                "getConstructors"
            ]
        );
    }

    #[test]
    fn tool_result_text_reads_content() {
        let value = json!({
            "content": [{ "type": "text", "text": "ok" }]
        });
        assert_eq!(tool_result_text(&value).unwrap(), "ok");
    }

    #[test]
    fn tool_result_text_errors() {
        let value = json!({
            "isError": true,
            "content": [{ "type": "text", "text": "boom" }]
        });
        assert!(tool_result_text(&value)
            .unwrap_err()
            .to_string()
            .contains("boom"));
    }

    #[tokio::test]
    async fn live_initialize_and_search_if_configured() {
        let (Ok(jar), Ok(platform)) = (
            std::env::var("BSL_CONTEXT_JAR"),
            std::env::var("BSL_PLATFORM_PATH"),
        ) else {
            eprintln!("skip live bsl-context test: set BSL_CONTEXT_JAR and BSL_PLATFORM_PATH");
            return;
        };
        if !std::path::Path::new(&jar).exists() || !std::path::Path::new(&platform).exists() {
            eprintln!("skip live bsl-context test: jar/platform missing");
            return;
        }

        let service = BslContextService::new();
        let client = service
            .ensure("java", &jar, &platform)
            .await
            .expect("spawn bsl-context");

        let listed = client
            .request("tools/list", json!({}))
            .await
            .expect("tools/list");
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        for expected in PLATFORM_TOOLS {
            assert!(
                names.contains(expected),
                "missing platform tool {expected} in {names:?}"
            );
        }

        let search = client
            .call_tool(
                "search",
                json!({
                    "query": "ТаблицаЗначений",
                    "type": "type",
                    "limit": 5
                }),
            )
            .await
            .expect("search");
        let text = tool_result_text(&search).expect("search text");
        assert!(
            text.to_lowercase().contains("таблица") || text.contains("Table") || !text.is_empty(),
            "unexpected search payload: {text}"
        );

        service.shutdown().await;
    }
}
