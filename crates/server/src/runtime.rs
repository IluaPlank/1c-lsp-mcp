use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use lsp_skill_core::BslContextService;
use lsp_skill_core::{AppConfig, AppPaths, Database, LspManager, RuntimeMetadata, ServiceError};

use crate::{api, mcp, web};

#[derive(Clone)]
pub struct ServerState {
    pub manager: Arc<LspManager>,
    pub paths: AppPaths,
    pub shutdown_token: CancellationToken,
}

pub struct StartedServer {
    runtime: RuntimeMetadata,
    paths: AppPaths,
    manager: Arc<LspManager>,
    bsl_context: Arc<BslContextService>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    shutdown_token: CancellationToken,
    task: JoinHandle<Result<()>>,
    mcp_tasks: Vec<JoinHandle<()>>,
}

impl StartedServer {
    pub async fn start(paths: AppPaths, config: AppConfig) -> Result<Self> {
        if let Some(existing) = RuntimeMetadata::read(&paths).await? {
            if existing.process_is_alive() {
                return Err(anyhow!(
                    "server already running at {} (pid {})",
                    existing.connect_url,
                    existing.pid
                ));
            }
            RuntimeMetadata::remove(&paths).await?;
        }

        let config = Arc::new(RwLock::new(config));
        let db = Database::open(&paths.db_path).await?;
        let manager = Arc::new(LspManager::load(paths.clone(), config, db).await?);
        let bsl_context = Arc::new(BslContextService::new());
        let shutdown_token = CancellationToken::new();
        let state = ServerState {
            manager: manager.clone(),
            paths: paths.clone(),
            shutdown_token: shutdown_token.clone(),
        };

        let current = manager.current_config().await;
        let bind = format!("{}:{}", current.listen_host, current.http_port);
        let listener = TcpListener::bind(&bind).await.map_err(|err| {
            anyhow!(ServiceError::PortInUse(format!(
                "failed to bind {bind}: {err}"
            )))
        })?;

        let runtime = RuntimeMetadata::new(
            std::process::id(),
            current.listen_host.clone(),
            current.http_port,
        );
        runtime.write(&paths).await?;

        let app = Router::new()
            .nest("/api", api::router())
            .merge(web::router())
            .with_state(state);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .context("axum server failed")
        });

        // Start unified MCP server if enabled (legacy dual flags migrate into one port)
        let mut mcp_tasks = Vec::new();
        if let Some(mcp_port) = current.effective_mcp_port() {
            let mcp_bind = format!("{}:{}", current.listen_host, mcp_port);
            match start_mcp_server(
                manager.clone(),
                bsl_context.clone(),
                &mcp_bind,
                shutdown_token.clone(),
            )
            .await
            {
                Ok(handle) => {
                    tracing::info!("MCP 1c (unified): http://{mcp_bind}/mcp");
                    mcp_tasks.push(handle);
                }
                Err(err) => {
                    tracing::error!("Failed to start unified MCP: {err}");
                }
            }
        }

        Ok(Self {
            runtime,
            paths,
            manager,
            shutdown_tx: Some(shutdown_tx),
            shutdown_token,
            task,
            mcp_tasks,
            bsl_context,
        })
    }

    pub fn connect_url(&self) -> &str {
        &self.runtime.connect_url
    }

    pub async fn wait_for_signal(self) -> Result<()> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let mut sigterm =
                signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;

            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result.context("failed to listen for ctrl+c")?;
                }
                _ = sigterm.recv() => {}
            }
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .context("failed to listen for ctrl+c")?;
        }

        // Second Ctrl+C forces immediate exit (shutdown may be slow
        // if Java processes don't respond to LSP shutdown/exit).
        tokio::spawn(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("Forced exit");
            std::process::exit(1);
        });

        self.shutdown().await
    }

    pub async fn shutdown(mut self) -> Result<()> {
        tracing::info!("shutting down...");

        // Сигнализируем SSE-стримам и MCP-серверам о завершении
        self.shutdown_token.cancel();

        self.manager.shutdown_all().await;
        self.bsl_context.shutdown().await;

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        // Даём серверу 3 секунды на graceful shutdown, потом прерываем
        match tokio::time::timeout(Duration::from_secs(3), self.task).await {
            Ok(result) => {
                result.context("server task join failed")??;
            }
            Err(_) => {
                tracing::warn!("server shutdown timed out, forcing exit");
            }
        }

        for task in self.mcp_tasks {
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
        }

        RuntimeMetadata::remove(&self.paths).await?;
        Ok(())
    }
}

async fn start_mcp_server(
    manager: Arc<LspManager>,
    bsl_context: Arc<BslContextService>,
    bind_addr: &str,
    shutdown_token: CancellationToken,
) -> Result<JoinHandle<()>> {
    let app = mcp::router(manager, bsl_context);
    let listener = TcpListener::bind(bind_addr).await.map_err(|err| {
        anyhow!(ServiceError::PortInUse(format!(
            "failed to bind MCP {bind_addr}: {err}"
        )))
    })?;

    Ok(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_token.cancelled().await;
            })
            .await
            .ok();
    }))
}
