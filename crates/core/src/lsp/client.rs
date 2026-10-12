use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, timeout};

use super::transport::{LspReader, LspWriter};

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
pub type NotificationHandler = Arc<dyn Fn(String, Value) + Send + Sync>;
pub type StderrHandler = Arc<dyn Fn(String) + Send + Sync>;

/// Default for ordinary navigation / diagnostics requests.
pub const LSP_TIMEOUT_DEFAULT: Duration = Duration::from_secs(45);
/// Interactive editor-like methods that must not jam the LS queue.
pub const LSP_TIMEOUT_INTERACTIVE: Duration = Duration::from_secs(10);
/// Heavy composite / workspace work.
pub const LSP_TIMEOUT_HEAVY: Duration = Duration::from_secs(120);
/// After client-side timeout, briefly wait for a late LS response before
/// releasing the gate. Interactive callers should kick/restart LS on hang.
const LSP_DRAIN_AFTER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct LspClient {
    sender: mpsc::Sender<Value>,
    pending: PendingMap,
    next_id: Arc<AtomicI64>,
    /// Serializes requests: at most one in-flight LSP request per process.
    /// Prevents completion timeouts from stacking and starving navigation.
    in_flight: Arc<Semaphore>,
}

impl LspClient {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_timeout(method, params, LSP_TIMEOUT_DEFAULT)
            .await
    }

    /// Waits until the language server answers or `cancel` becomes true.
    /// There is no timer: the caller decides how long to wait.
    pub async fn request_until_cancel(
        &self,
        method: &str,
        params: Value,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Value> {
        let permit = self
            .in_flight
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("LSP request semaphore closed"))?;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self.sender.send(message).await {
            self.pending.lock().await.remove(&id);
            drop(permit);
            return Err(anyhow!("LSP writer channel closed: {err}"));
        }

        if *cancel.borrow() {
            self.pending.lock().await.remove(&id);
            drop(permit);
            bail!("LSP request cancelled: {method}");
        }

        tokio::select! {
            response = &mut rx => {
                drop(permit);
                match response {
                    Ok(response) => {
                        if let Some(error) = response.get("error") {
                            bail!("LSP error: {error}");
                        }
                        Ok(response.get("result").cloned().unwrap_or(Value::Null))
                    }
                    Err(_) => {
                        self.pending.lock().await.remove(&id);
                        bail!("LSP response channel dropped for {method}")
                    }
                }
            }
            changed = cancel.changed() => {
                let _ = changed;
                self.pending.lock().await.remove(&id);
                drop(permit);
                bail!("LSP request cancelled: {method}")
            }
        }
    }

    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout_dur: Duration,
    ) -> Result<Value> {
        let permit = self
            .in_flight
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("LSP request semaphore closed"))?;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self.sender.send(message).await {
            self.pending.lock().await.remove(&id);
            drop(permit);
            return Err(anyhow!("LSP writer channel closed: {err}"));
        }

        tokio::select! {
            response = &mut rx => {
                drop(permit);
                match response {
                    Ok(response) => {
                        if let Some(error) = response.get("error") {
                            bail!("LSP error: {error}");
                        }
                        Ok(response.get("result").cloned().unwrap_or(Value::Null))
                    }
                    Err(_) => {
                        self.pending.lock().await.remove(&id);
                        bail!("LSP response channel dropped for {method}")
                    }
                }
            }
            _ = sleep(timeout_dur) => {
                let _ = self
                    .notify("$/cancelRequest", json!({ "id": id }))
                    .await;
                Self::spawn_drain_rx(method.to_string(), id, rx, permit);
                bail!(
                    "LSP request timed out after {}ms: {method}",
                    timeout_dur.as_millis()
                )
            }
        }
    }

    fn spawn_drain_rx(
        method: String,
        id: i64,
        rx: oneshot::Receiver<Value>,
        permit: OwnedSemaphorePermit,
    ) {
        tokio::spawn(async move {
            match timeout(LSP_DRAIN_AFTER_TIMEOUT, rx).await {
                Ok(Ok(_)) => {
                    tracing::debug!("LSP late response drained for {method} id={id}");
                }
                Ok(Err(_)) => {
                    tracing::debug!("LSP drain channel dropped for {method} id={id}");
                }
                Err(_) => {
                    tracing::warn!(
                        "LSP drain timed out for {method} id={id}; releasing gate anyway"
                    );
                }
            }
            drop(permit);
        });
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
            .map_err(|_| anyhow!("LSP writer channel closed"))?;
        Ok(())
    }
}

pub async fn spawn_lsp_server(
    java_path: &str,
    jar_path: &str,
    jvm_args: &str,
    bsl_config_path: Option<&str>,
    notification_handler: NotificationHandler,
    stderr_handler: StderrHandler,
) -> Result<(LspClient, Child)> {
    let mut command = Command::new(java_path);
    for arg in shlex::split(jvm_args).unwrap_or_default() {
        command.arg(arg);
    }
    command.arg("-jar").arg(jar_path).arg("lsp");
    if let Some(config_path) = bsl_config_path {
        command.arg("--configuration").arg(config_path);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
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
    let (tx, mut rx) = mpsc::channel::<Value>(64);

    tokio::spawn(async move {
        let mut writer = LspWriter::new(stdin);
        while let Some(message) = rx.recv().await {
            if let Err(err) = writer.send(&message).await {
                tracing::error!("LSP write error: {err}");
                break;
            }
        }
    });

    let pending_for_reader = pending.clone();
    let tx_for_reader = tx.clone();
    tokio::spawn(async move {
        let mut reader = LspReader::new(stdout);
        loop {
            match reader.recv().await {
                Ok(message) => {
                    if let Some(id) = message.get("id") {
                        if message.get("method").is_some() {
                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": id.clone(),
                                "result": null,
                            });
                            let _ = tx_for_reader.send(response).await;
                        } else {
                            let request_id = id.as_i64().unwrap_or(-1);
                            let mut pending = pending_for_reader.lock().await;
                            if let Some(tx) = pending.remove(&request_id) {
                                let _ = tx.send(message);
                            }
                        }
                    } else {
                        let method = message
                            .get("method")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        let handler = notification_handler.clone();
                        tokio::spawn(async move {
                            handler(method, params);
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!("LSP reader stopped: {err}");
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let handler = stderr_handler.clone();
            tokio::spawn(async move {
                handler(line);
            });
        }
    });

    Ok((
        LspClient {
            sender: tx,
            pending,
            next_id: Arc::new(AtomicI64::new(1)),
            in_flight: Arc::new(Semaphore::new(1)),
        },
        child,
    ))
}
