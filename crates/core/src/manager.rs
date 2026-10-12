use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result as AnyResult;
use chrono::Utc;
use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tokio::time::sleep;
use url::Url;
use uuid::Uuid;

use crate::cache::{CacheKey, ResponseCache};
use crate::config::AppConfig;
use crate::db::Database;
use crate::error::ServiceError;
use crate::events::LspEvent;
use crate::logging::{append_project_log, clear_project_logs, tail_project_log};
use crate::lsp::{
    spawn_lsp_server, LspClient, NotificationHandler, StderrHandler, LSP_TIMEOUT_HEAVY,
    LSP_TIMEOUT_INTERACTIVE,
};
use crate::models::{
    IndexingProgress, ProjectSnapshot, ProjectStatus, ProjectUpsert, StoredProject,
};
use crate::paths::AppPaths;
use crate::watcher::ProjectWatcher;

pub(crate) type SharedProject = Arc<RwLock<ProjectState>>;

#[derive(Debug, Clone, Copy)]
struct OpenedDoc {
    version: u32,
    content_hash: u64,
}

struct OpenedFileContext {
    project: SharedProject,
    uri: String,
    client: LspClient,
    content_hash: u64,
    status: ProjectStatus,
    /// Relative path as passed by the caller. Cache keys use this, not the file URI.
    file_path: String,
}

pub(crate) struct ProjectState {
    pub(crate) config: StoredProject,
    status: ProjectStatus,
    client: Option<LspClient>,
    child: Option<Arc<Mutex<Child>>>,
    server_caps: Option<ServerCaps>,
    diagnostics: HashMap<String, Value>,
    /// Tracks open docs so we avoid flooding BSL LS with identical didChange.
    opened_files: HashMap<String, OpenedDoc>,
    /// Finished LSP tool responses. Cleared on any `.bsl`/`.os` change and on restart.
    response_cache: ResponseCache,
    progress: IndexingProgress,
    /// Token of the main progress (the one that sends "report" events).
    progress_token: Option<String>,
    watcher: Option<ProjectWatcher>,
    /// Signalled by `stop_project` while `initialize` is still running.
    startup_cancel: Option<watch::Sender<bool>>,
}

impl ProjectState {
    fn from_config(config: StoredProject) -> Self {
        Self {
            config,
            status: ProjectStatus::Stopped,
            client: None,
            child: None,
            server_caps: None,
            diagnostics: HashMap::new(),
            opened_files: HashMap::new(),
            response_cache: ResponseCache::new(),
            progress: IndexingProgress::default(),
            progress_token: None,
            watcher: None,
            startup_cancel: None,
        }
    }

    fn snapshot(&self) -> ProjectSnapshot {
        ProjectSnapshot {
            project: self.config.clone(),
            status: self.status.info(),
            progress: self.progress.clone(),
        }
    }
}

#[derive(Clone)]
pub struct LspManager {
    projects: Arc<RwLock<HashMap<String, SharedProject>>>,
    db: Database,
    paths: AppPaths,
    config: Arc<RwLock<AppConfig>>,
    event_tx: broadcast::Sender<LspEvent>,
}

#[derive(Clone, Debug)]
struct ServerCaps {
    supports_pull_diagnostics: bool,
    supports_call_hierarchy: bool,
    supports_did_save: bool,
}

impl LspManager {
    pub async fn load(
        paths: AppPaths,
        config: Arc<RwLock<AppConfig>>,
        db: Database,
    ) -> AnyResult<Self> {
        let mut projects = HashMap::new();
        for project in db.list_projects().await? {
            projects.insert(
                project.id.clone(),
                Arc::new(RwLock::new(ProjectState::from_config(project))),
            );
        }

        let (event_tx, _) = broadcast::channel(256);
        Ok(Self {
            projects: Arc::new(RwLock::new(projects)),
            db,
            paths,
            config,
            event_tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LspEvent> {
        self.event_tx.subscribe()
    }

    pub async fn current_config(&self) -> AppConfig {
        self.config.read().await.clone()
    }

    pub async fn replace_config(&self, next: AppConfig, restart_required: bool) {
        *self.config.write().await = next;
        let _ = self
            .event_tx
            .send(LspEvent::SettingsChanged { restart_required });
    }

    pub async fn list_projects(&self) -> Vec<ProjectSnapshot> {
        let handles: Vec<SharedProject> = self.projects.read().await.values().cloned().collect();
        let mut snapshots = Vec::with_capacity(handles.len());
        for project in handles {
            snapshots.push(project.read().await.snapshot());
        }
        snapshots
    }

    pub async fn get_project(&self, id: &str) -> Result<ProjectSnapshot, ServiceError> {
        let project = self.project_handle(id).await?;
        let snapshot = project.read().await.snapshot();
        Ok(snapshot)
    }

    pub async fn create_project(
        &self,
        input: ProjectUpsert,
    ) -> Result<ProjectSnapshot, ServiceError> {
        validate_bsl_config(&input.bsl_config)?;
        let now = Utc::now().to_rfc3339();
        let project = StoredProject {
            id: Uuid::new_v4().to_string(),
            name: validate_name(&input.name)?,
            root_path: canonical_root_path(&input.root_path)?,
            project_root_path: canonical_root_path(&input.project_root_path)?,
            jvm_args: input.jvm_args.trim().to_string(),
            bsl_config: input.bsl_config.clone(),
            debug: input.debug,
            created_at: now.clone(),
            updated_at: now,
        };

        self.db
            .insert_project(&project)
            .await
            .map_err(|err| map_create_project_error(&err))?;

        let state = Arc::new(RwLock::new(ProjectState::from_config(project.clone())));
        self.projects
            .write()
            .await
            .insert(project.id.clone(), state.clone());

        let _ = self.event_tx.send(LspEvent::ProjectsChanged);
        let snapshot = state.read().await.snapshot();
        Ok(snapshot)
    }

    pub async fn update_project(
        &self,
        id: &str,
        input: ProjectUpsert,
    ) -> Result<ProjectSnapshot, ServiceError> {
        validate_bsl_config(&input.bsl_config)?;
        let project = self.project_handle(id).await?;
        {
            let state = project.read().await;
            if !state.status.is_stopped() {
                return Err(ServiceError::InvalidRequest(
                    "Чтобы изменить проект, сначала остановите его.".to_string(),
                ));
            }
        }

        let updated = {
            let mut state = project.write().await;
            state.config.name = validate_name(&input.name)?;
            state.config.root_path = canonical_root_path(&input.root_path)?;
            state.config.project_root_path = canonical_root_path(&input.project_root_path)?;
            state.config.jvm_args = input.jvm_args.trim().to_string();
            state.config.bsl_config = input.bsl_config.clone();
            state.config.debug = input.debug;
            state.config.updated_at = Utc::now().to_rfc3339();
            state.config.clone()
        };

        self.db
            .update_project(&updated)
            .await
            .map_err(|err| map_update_project_error(&err))?;

        let _ = self.event_tx.send(LspEvent::ProjectsChanged);
        let snapshot = project.read().await.snapshot();
        Ok(snapshot)
    }

    pub async fn delete_project(&self, id: &str) -> Result<(), ServiceError> {
        let project = self.project_handle(id).await?;
        {
            let state = project.read().await;
            if !state.status.is_stopped() {
                return Err(ServiceError::InvalidRequest(
                    "Нельзя удалить запущенный проект. Сначала остановите его.".to_string(),
                ));
            }
        }

        self.db
            .delete_project(id)
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))?;
        self.projects.write().await.remove(id);
        let _ = self.event_tx.send(LspEvent::ProjectsChanged);
        Ok(())
    }

    pub async fn start_project(&self, id: &str) -> Result<ProjectSnapshot, ServiceError> {
        let project = self.project_handle(id).await?;
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let (root_path, jvm_args, bsl_config, project_id) = {
            let mut state = project.write().await;
            begin_start(&mut state, cancel_tx)?
        };
        self.emit_status(id, &project).await;

        let settings = self.current_config().await;
        if settings.jar_path.trim().is_empty() {
            self.set_error(
                id,
                &project,
                "Не указан путь к JAR-файлу BSL Language Server.",
            )
            .await;
            return Err(ServiceError::InvalidRequest(
                "Не указан путь к JAR-файлу BSL Language Server.".to_string(),
            ));
        }

        let notification_handler = project_notification_handler(
            project.clone(),
            self.paths.logs_dir.clone(),
            self.event_tx.clone(),
            id.to_string(),
        );
        let stderr_handler = project_stderr_handler(
            project.clone(),
            self.paths.logs_dir.clone(),
            self.event_tx.clone(),
            id.to_string(),
        );

        // Write per-project BSL config (always — use default if user didn't set one)
        let bsl_config_content = effective_bsl_config(&bsl_config);

        let bsl_config_file = {
            let config_dir = &self.paths.bsl_configs_dir;
            let config_file = config_dir.join(format!("{project_id}.json"));
            if let Err(err) = tokio::fs::create_dir_all(config_dir).await {
                self.set_error(
                    id,
                    &project,
                    &format!("Не удалось создать каталог конфигов: {err}"),
                )
                .await;
                return Err(ServiceError::Internal(format!(
                    "Не удалось создать каталог конфигов: {err}"
                )));
            }
            if let Err(err) = tokio::fs::write(&config_file, bsl_config_content).await {
                self.set_error(
                    id,
                    &project,
                    &format!("Не удалось записать конфигурацию BSL: {err}"),
                )
                .await;
                return Err(ServiceError::Internal(format!(
                    "Не удалось записать конфигурацию BSL: {err}"
                )));
            }
            Some(config_file)
        };

        let bsl_config_path_str = bsl_config_file
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        let (client, child) = match spawn_lsp_server(
            "java",
            &settings.jar_path,
            &jvm_args,
            bsl_config_path_str.as_deref(),
            notification_handler,
            stderr_handler,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                let message = if err.to_string().contains("No such file or directory") {
                    "Java не найдена в PATH.".to_string()
                } else {
                    format!("Не удалось запустить BSL Language Server: {err}")
                };
                self.set_error(id, &project, &message).await;
                return if message.contains("Java не найдена") {
                    Err(ServiceError::JavaNotFound(message))
                } else {
                    Err(ServiceError::Internal(message))
                };
            }
        };
        let child = Arc::new(Mutex::new(child));
        {
            let mut state = project.write().await;
            if startup_was_cancelled(&state) {
                drop(state);
                kill_child(&child).await;
                return self.finish_user_stop(id, &project).await;
            }
            state.child = Some(child.clone());
        }

        // Log startup info
        {
            let jar_display = normalize_slashes(&settings.jar_path);
            let mut cmd_line = if jvm_args.is_empty() {
                format!("java -jar {jar_display} lsp")
            } else {
                format!("java {jvm_args} -jar {jar_display} lsp")
            };
            if let Some(ref cfg_path) = bsl_config_path_str {
                cmd_line.push_str(&format!(" --configuration {}", normalize_slashes(cfg_path)));
            }
            let root_uri = Url::from_directory_path(&root_path)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| root_path.clone());

            let mut lines = vec![
                "─── Запуск BSL Language Server ───".to_string(),
                format!("Командная строка: {cmd_line}"),
                format!("Корневой каталог: {}", normalize_slashes(&root_path)),
                format!("rootUri: {root_uri}"),
            ];

            if let Some(ref cfg_path) = bsl_config_path_str {
                match tokio::fs::read_to_string(cfg_path).await {
                    Ok(content) => {
                        lines.push(format!(
                            "Конфигурация BSL ({}):",
                            normalize_slashes(cfg_path),
                        ));
                        lines.push(content);
                    }
                    Err(_) => {
                        lines.push("Конфигурация BSL: ошибка чтения файла".to_string());
                    }
                }
            } else {
                let config_path = PathBuf::from(&root_path).join(".bsl-language-server.json");
                match tokio::fs::read_to_string(&config_path).await {
                    Ok(content) => {
                        lines.push(format!(
                            "Конфигурация BSL ({}):",
                            normalize_slashes(&config_path.display().to_string()),
                        ));
                        lines.push(content);
                    }
                    Err(_) => {
                        lines.push(
                            "Конфигурация BSL (.bsl-language-server.json): не найдена".to_string(),
                        );
                    }
                }
            }

            for line in lines {
                let _ =
                    append_project_log(self.paths.logs_dir.clone(), id.to_string(), line.clone())
                        .await;
                let _ = self.event_tx.send(LspEvent::LogLine {
                    id: id.to_string(),
                    line,
                });
            }
        }

        let server_caps = match initialize_client(&client, &root_path, cancel_rx).await {
            Ok(caps) => caps,
            Err(err) => {
                let cancelled = {
                    let state = project.read().await;
                    startup_was_cancelled(&state)
                };
                kill_child(&child).await;
                if cancelled || err.to_string().contains("cancelled") {
                    return self.finish_user_stop(id, &project).await;
                }
                self.set_error(id, &project, &err.to_string()).await;
                return Err(ServiceError::Internal(
                    "Не удалось инициализировать LSP-сервер.".to_string(),
                ));
            }
        };

        {
            let state = project.read().await;
            if startup_was_cancelled(&state) {
                drop(state);
                kill_child(&child).await;
                return self.finish_user_stop(id, &project).await;
            }
        }

        let caps_line = format!(
            "Server capabilities: pull_diagnostics={}, call_hierarchy={}, did_save={}",
            server_caps.supports_pull_diagnostics,
            server_caps.supports_call_hierarchy,
            server_caps.supports_did_save
        );
        let _ = append_project_log(
            self.paths.logs_dir.clone(),
            id.to_string(),
            caps_line.clone(),
        )
        .await;
        let _ = self.event_tx.send(LspEvent::LogLine {
            id: id.to_string(),
            line: caps_line,
        });

        let watcher = match crate::watcher::start_project_watcher(
            &root_path,
            client.clone(),
            id.to_string(),
            project.clone(),
            self.paths.logs_dir.clone(),
            self.event_tx.clone(),
        ) {
            Ok(w) => {
                tracing::info!("File watcher started for project {id}");
                Some(w)
            }
            Err(err) => {
                tracing::warn!("File watcher failed to start for project {id}: {err}");
                None
            }
        };

        {
            let mut state = project.write().await;
            if startup_was_cancelled(&state) {
                drop(state);
                kill_child(&child).await;
                return self.finish_user_stop(id, &project).await;
            }
            state.startup_cancel = None;
            state.client = Some(client);
            state.child = Some(child.clone());
            state.watcher = watcher;
            state.server_caps = Some(server_caps);
            state.status = ProjectStatus::WarmingUp;
        }
        self.emit_status(id, &project).await;

        tokio::spawn(watch_project_process(
            project.clone(),
            self.event_tx.clone(),
            id.to_string(),
            child,
        ));

        let snapshot = project.read().await.snapshot();
        Ok(snapshot)
    }

    pub async fn stop_project(&self, id: &str) -> Result<ProjectSnapshot, ServiceError> {
        let project = self.project_handle(id).await?;
        let (client, child, cancel) = {
            let state = project.read().await;
            (
                state.client.clone(),
                state.child.clone(),
                state.startup_cancel.clone(),
            )
        };

        if let Some(cancel) = cancel {
            let _ = cancel.send(true);
        }

        // During initialize there is no LSP client yet. Shutdown would wait
        // behind that request, so the process is killed immediately.
        if client.is_none() {
            if let Some(child) = &child {
                kill_child(child).await;
            }
        } else if let Some(client) = client {
            let _ = client.request("shutdown", Value::Null).await;
            let _ = client.notify("exit", Value::Null).await;
            if let Some(child) = child {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let exited = {
                        let mut child = child.lock().await;
                        match child.try_wait() {
                            Ok(Some(_)) => true,
                            Ok(None) => false,
                            Err(_) => true,
                        }
                    };

                    if exited {
                        break;
                    }

                    if Instant::now() >= deadline {
                        let _ = child.lock().await.kill().await;
                        break;
                    }

                    sleep(Duration::from_millis(250)).await;
                }
            }
        }

        {
            let mut state = project.write().await;
            apply_stop(&mut state);
        }

        self.emit_status(id, &project).await;
        let snapshot = project.read().await.snapshot();
        Ok(snapshot)
    }

    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = self.projects.read().await.keys().cloned().collect();
        for id in ids {
            let _ = self.stop_project(&id).await;
        }
    }

    pub async fn diagnostics(&self, id: &str, file_path: &str) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let (supports_pull, supports_save) = {
            let state = ctx.project.read().await;
            let caps = state.server_caps.as_ref();
            (
                caps.map(|c| c.supports_pull_diagnostics).unwrap_or(false),
                caps.map(|c| c.supports_did_save).unwrap_or(false),
            )
        };

        if !supports_pull {
            return Err(ServiceError::Internal(
                "BSL Language Server не поддерживает pull diagnostics (textDocument/diagnostic). \
                 Обновите bsl-language-server до актуальной версии."
                    .to_string(),
            ));
        }

        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        self.with_file_cache(&ctx, "diagnostics", None, None, "", || {
            let this = this.clone();
            let id = id.clone();
            let file_path_owned = file_path_owned.clone();
            let uri = uri.clone();
            let client = client.clone();
            async move {
                if supports_save {
                    let _ = client
                        .notify(
                            "textDocument/didSave",
                            json!({
                                "textDocument": { "uri": uri }
                            }),
                        )
                        .await;
                }

                this.log(
                    &id,
                    format!(">> textDocument/diagnostic: {file_path_owned}"),
                )
                .await;

                let result = client
                    .request(
                        "textDocument/diagnostic",
                        json!({
                            "textDocument": { "uri": uri }
                        }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()))?;

                let items = result.get("items").cloned().unwrap_or_else(|| json!([]));
                let count = items.as_array().map(|a| a.len()).unwrap_or(0);

                this.log(
                    &id,
                    format!("<< textDocument/diagnostic: {count} диагностик"),
                )
                .await;

                Ok(json!({ "uri": uri, "diagnostics": items }))
            }
        })
        .await
    }

    pub async fn symbols(&self, id: &str, file_path: &str) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        self.with_file_cache(&ctx, "symbols", None, None, "", || {
            let this = this.clone();
            let id = id.clone();
            let file_path_owned = file_path_owned.clone();
            let uri = uri.clone();
            let client = client.clone();
            async move {
                this.log(&id, format!(">> symbols: {file_path_owned}"))
                    .await;

                let result = client
                    .request(
                        "textDocument/documentSymbol",
                        json!({ "textDocument": { "uri": uri } }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()));

                match &result {
                    Ok(value) => {
                        this.log(&id, format!("<< symbols: {value}")).await;
                    }
                    Err(err) => {
                        this.log(&id, format!("<< symbols ошибка: {err}")).await;
                    }
                }

                result
            }
        })
        .await
    }

    pub async fn references(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        self.with_file_cache(&ctx, "references", Some(line), Some(character), "", || {
            let this = this.clone();
            let id = id.clone();
            let file_path_owned = file_path_owned.clone();
            let uri = uri.clone();
            let client = client.clone();
            async move {
                this.log(
                    &id,
                    format!(">> references: {file_path_owned} ({line}:{character})"),
                )
                .await;

                let result = client
                    .request(
                        "textDocument/references",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": { "line": line, "character": character },
                            "context": { "includeDeclaration": true }
                        }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()));

                match &result {
                    Ok(value) => {
                        this.log(&id, format!("<< references: {value}")).await;
                    }
                    Err(err) => {
                        this.log(&id, format!("<< references ошибка: {err}")).await;
                    }
                }

                result
            }
        })
        .await
    }

    pub async fn definition(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        self.with_file_cache(&ctx, "definition", Some(line), Some(character), "", || {
            let this = this.clone();
            let id = id.clone();
            let file_path_owned = file_path_owned.clone();
            let uri = uri.clone();
            let client = client.clone();
            async move {
                this.log(
                    &id,
                    format!(">> definition: {file_path_owned} ({line}:{character})"),
                )
                .await;

                let result = client
                    .request(
                        "textDocument/definition",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": { "line": line, "character": character }
                        }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()));

                match &result {
                    Ok(value) => {
                        this.log(&id, format!("<< definition: {value}")).await;
                    }
                    Err(err) => {
                        this.log(&id, format!("<< definition ошибка: {err}")).await;
                    }
                }

                result
            }
        })
        .await
    }

    pub async fn hover(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();

        self.log(id, format!(">> hover: {file_path} ({line}:{character})"))
            .await;

        let result = client
            .request_timeout(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()));

        match &result {
            Ok(value) => {
                self.log(id, format!("<< hover: {value}")).await;
            }
            Err(err) => {
                self.log(id, format!("<< hover ошибка: {err}")).await;
            }
        }

        result
    }

    pub async fn completion(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();

        self.log(
            id,
            format!(">> completion: {file_path} ({line}:{character})"),
        )
        .await;

        let result = client
            .request_timeout(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": {
                        "triggerKind": 1
                    }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()));

        if matches!(&result, Err(ServiceError::Internal(msg)) if msg.contains("timed out")) {
            self.kick_lsp_after_hang(id, "completion").await;
        }

        match &result {
            Ok(value) => {
                self.log(id, format!("<< completion: {value}")).await;
            }
            Err(err) => {
                self.log(id, format!("<< completion ошибка: {err}")).await;
            }
        }

        result
    }

    pub async fn code_actions(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();

        self.log(
            id,
            format!(">> code_actions: {file_path} ({line}:{character})"),
        )
        .await;

        let result = client
            .request_timeout(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": line, "character": character },
                        "end": { "line": line, "character": character }
                    },
                    "context": {
                        "diagnostics": []
                    }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()));

        match &result {
            Ok(value) => {
                self.log(id, format!("<< code_actions: {value}")).await;
            }
            Err(err) => {
                self.log(id, format!("<< code_actions ошибка: {err}")).await;
            }
        }

        result
    }

    pub async fn prepare_rename(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();

        self.log(
            id,
            format!(">> prepare_rename: {file_path} ({line}:{character})"),
        )
        .await;

        let result = client
            .request_timeout(
                "textDocument/prepareRename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()));

        match &result {
            Ok(value) => {
                self.log(id, format!("<< prepare_rename: {value}")).await;
            }
            Err(err) => {
                self.log(id, format!("<< prepare_rename ошибка: {err}"))
                    .await;
            }
        }

        result
    }

    pub async fn rename(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();

        self.log(
            id,
            format!(">> rename: {file_path} ({line}:{character}) -> {new_name}"),
        )
        .await;

        let result = client
            .request(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "newName": new_name
                }),
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()));

        match &result {
            Ok(value) => {
                self.log(id, format!("<< rename: {value}")).await;
            }
            Err(err) => {
                self.log(id, format!("<< rename ошибка: {err}")).await;
            }
        }

        result
    }

    pub async fn incoming_calls(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        self.cached_call_direction(
            id,
            file_path,
            line,
            character,
            "incoming_calls",
            "callHierarchy/incomingCalls",
        )
        .await
    }

    pub async fn outgoing_calls(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        self.cached_call_direction(
            id,
            file_path,
            line,
            character,
            "outgoing_calls",
            "callHierarchy/outgoingCalls",
        )
        .await
    }

    async fn cached_call_direction(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
        tool: &'static str,
        method: &'static str,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let supports = ctx
            .project
            .read()
            .await
            .server_caps
            .as_ref()
            .map(|c| c.supports_call_hierarchy)
            .unwrap_or(false);

        if !supports {
            return Err(ServiceError::Internal(
                "BSL Language Server не поддерживает Call Hierarchy. \
                 Обновите bsl-language-server до актуальной версии."
                    .to_string(),
            ));
        }

        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        self.with_file_cache(&ctx, tool, Some(line), Some(character), "", || {
            let this = this.clone();
            let id = id.clone();
            let file_path_owned = file_path_owned.clone();
            let uri = uri.clone();
            let client = client.clone();
            async move {
                this.log(
                    &id,
                    format!(">> {tool}: {file_path_owned} ({line}:{character})"),
                )
                .await;

                let items = client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": { "line": line, "character": character }
                        }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()))?;

                let item = match items.as_array().and_then(|arr| arr.first()) {
                    Some(item) => item.clone(),
                    None => {
                        this.log(
                            &id,
                            format!("<< {tool}: null (позиция не указывает на процедуру/функцию)"),
                        )
                        .await;
                        return Ok(Value::Null);
                    }
                };

                let result = client
                    .request(method, json!({ "item": item }))
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()))?;

                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                this.log(&id, format!("<< {tool}: {count} результатов"))
                    .await;

                Ok(result)
            }
        })
        .await
    }

    async fn prepare_call_item(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<(LspClient, Value), ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let project = ctx.project.clone();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        let supports = project
            .read()
            .await
            .server_caps
            .as_ref()
            .map(|c| c.supports_call_hierarchy)
            .unwrap_or(false);
        if !supports {
            return Err(ServiceError::Internal(
                "BSL Language Server не поддерживает Call Hierarchy.".to_string(),
            ));
        }
        let items = client
            .request(
                "textDocument/prepareCallHierarchy",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))?;
        let item = items
            .as_array()
            .and_then(|arr| arr.first())
            .cloned()
            .ok_or_else(|| {
                ServiceError::Internal(
                    "Позиция не указывает на процедуру/функцию (prepareCallHierarchy пуст)"
                        .to_string(),
                )
            })?;
        Ok((client, item))
    }

    pub async fn call_graph(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
        depth_up: u32,
        depth_down: u32,
        max_nodes: usize,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let params = format!("{depth_up},{depth_down},{max_nodes}");
        self.with_file_cache(
            &ctx,
            "call_graph",
            Some(line),
            Some(character),
            &params,
            || {
                let this = this.clone();
                let id = id.clone();
                let file_path_owned = file_path_owned.clone();
                async move {
                    let (client, root_item) = this
                        .prepare_call_item(&id, &file_path_owned, line, character)
                        .await?;
                    this.log(
                        &id,
                        format!(
                            ">> call_graph: {file_path_owned} ({line}:{character}) up={depth_up} down={depth_down} max={max_nodes}"
                        ),
                    )
                    .await;

                    let limits = crate::analysis::CallGraphLimits {
                        depth_up,
                        depth_down,
                        max_nodes,
                    };
                    let client2 = client.clone();
                    let graph = crate::analysis::build_call_graph_json(
                        root_item,
                        limits,
                        move |item, direction| {
                            let client = client2.clone();
                            async move {
                                let method = if direction == "up" {
                                    "callHierarchy/incomingCalls"
                                } else {
                                    "callHierarchy/outgoingCalls"
                                };
                                let result = client
                                    .request_timeout(
                                        method,
                                        json!({ "item": item }),
                                        LSP_TIMEOUT_HEAVY,
                                    )
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok(result.as_array().cloned().unwrap_or_default())
                            }
                        },
                    )
                    .await;

                    this.log(
                        &id,
                        format!(
                            "<< call_graph: nodes={} cycles={}",
                            graph
                                .get("total_nodes")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            graph
                                .get("cycles_found")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                        ),
                    )
                    .await;
                    Ok(graph)
                }
            },
        )
        .await
    }

    pub async fn symbol_impact(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let (client, item) = self
            .prepare_call_item(id, file_path, line, character)
            .await?;
        let symbol_name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let callers = client
            .request("callHierarchy/incomingCalls", json!({ "item": item }))
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))?;
        let callers_arr = callers.as_array().cloned().unwrap_or_default();

        let refs = self
            .references(id, file_path, line, character)
            .await
            .unwrap_or(Value::Array(vec![]));

        Ok(crate::analysis::build_symbol_impact_json(
            &symbol_name,
            file_path,
            line,
            character,
            &callers_arr,
            &refs,
        ))
    }

    pub async fn complexity(&self, id: &str, file_path: &str) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let this = self.clone();
        let id = id.to_string();
        let file_path_owned = file_path.to_string();
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        self.with_file_cache(&ctx, "complexity", None, None, "", || {
            let this = this.clone();
            let id = id.clone();
            let file_path_owned = file_path_owned.clone();
            let uri = uri.clone();
            let client = client.clone();
            async move {
                this.log(&id, format!(">> complexity/codeLens: {file_path_owned}"))
                    .await;

                let lenses = client
                    .request(
                        "textDocument/codeLens",
                        json!({ "textDocument": { "uri": uri } }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()))?;

                let mut resolved = Vec::new();
                if let Some(arr) = lenses.as_array() {
                    for lens in arr {
                        if lens.get("command").is_some() {
                            resolved.push(lens.clone());
                            continue;
                        }
                        match client.request("codeLens/resolve", lens.clone()).await {
                            Ok(v) => resolved.push(v),
                            Err(_) => resolved.push(lens.clone()),
                        }
                    }
                }

                let rows = crate::analysis::parse_complexity_lenses(&resolved);
                Ok(crate::analysis::complexity_rows_to_json(&rows))
            }
        })
        .await
    }

    pub async fn quality_diagnostics(
        &self,
        id: &str,
        file_path: &str,
        categories: &[String],
    ) -> Result<Value, ServiceError> {
        let diags = self.diagnostics(id, file_path).await?;
        let arr = lsp_diagnostic_items(&diags);
        let wanted: Vec<&str> = if categories.is_empty() {
            vec!["security", "performance", "sql"]
        } else {
            categories.iter().map(|s| s.as_str()).collect()
        };
        Ok(crate::analysis::filter_quality_diagnostics(&arr, &wanted))
    }

    pub async fn module_health(&self, id: &str, file_path: &str) -> Result<Value, ServiceError> {
        let complexity = self.complexity(id, file_path).await?;
        let quality = self
            .quality_diagnostics(id, file_path, &[])
            .await
            .unwrap_or(json!({ "diagnostics": [], "count": 0 }));
        Ok(crate::analysis::merge_module_health(&complexity, &quality))
    }

    pub async fn signature_help(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        let result = client
            .request_timeout(
                "textDocument/signatureHelp",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": {
                        "triggerKind": 1,
                        "isRetrigger": false
                    }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()));
        if matches!(&result, Err(ServiceError::Internal(msg)) if msg.contains("timed out")) {
            self.kick_lsp_after_hang(id, "signatureHelp").await;
        }
        result
    }

    pub async fn inlay_hints(
        &self,
        id: &str,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        client
            .request_timeout(
                "textDocument/inlayHint",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start_line, "character": 0 },
                        "end": { "line": end_line, "character": 0 }
                    }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))
    }

    pub async fn selection_range(
        &self,
        id: &str,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        client
            .request_timeout(
                "textDocument/selectionRange",
                json!({
                    "textDocument": { "uri": uri },
                    "positions": [{ "line": line, "character": character }]
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))
    }

    pub async fn semantic_tokens(
        &self,
        id: &str,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<Value, ServiceError> {
        let ctx = self.ensure_file_opened(id, file_path).await?;
        let uri = ctx.uri.clone();
        let client = ctx.client.clone();
        client
            .request_timeout(
                "textDocument/semanticTokens/range",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start_line, "character": 0 },
                        "end": { "line": end_line, "character": 0 }
                    }
                }),
                LSP_TIMEOUT_INTERACTIVE,
            )
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))
    }

    pub async fn get_range_content(
        &self,
        id: &str,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<Value, ServiceError> {
        let project = self.project_handle(id).await?;
        let abs = {
            let state = project.read().await;
            let root = PathBuf::from(&state.config.project_root_path);
            let candidate = root.join(file_path);
            if !candidate.starts_with(&root) {
                return Err(ServiceError::InvalidRequest(
                    "file_path вне project_root_path".to_string(),
                ));
            }
            candidate
        };
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let lines: Vec<&str> = content.lines().collect();
        let start = start_line as usize;
        let end = (end_line as usize).min(lines.len().saturating_sub(1));
        if start >= lines.len() {
            return Ok(json!({ "text": "", "start_line": start_line, "end_line": end_line }));
        }
        let slice = lines[start..=end].join("\n");
        Ok(json!({
            "text": slice,
            "start_line": start_line,
            "end_line": end as u32,
            "file_path": file_path
        }))
    }

    pub async fn symbol_explore(
        &self,
        id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Value, ServiceError> {
        let symbols = self.workspace_symbols(id, query).await?;
        let arr = symbols.as_array().cloned().unwrap_or_default();
        let limited: Vec<Value> = arr.into_iter().take(limit.max(1)).collect();
        Ok(json!({
            "query": query,
            "count": limited.len(),
            "symbols": limited
        }))
    }

    pub async fn project_analysis(
        &self,
        id: &str,
        analysis_type: &str,
        query: &str,
    ) -> Result<Value, ServiceError> {
        match analysis_type {
            "workspace_symbols" => self.symbol_explore(id, query, 50).await,
            "file_analysis" => {
                let symbols = self.symbols(id, query).await?;
                let diags = self.diagnostics(id, query).await.unwrap_or(json!([]));
                Ok(json!({
                    "file_path": query,
                    "symbols": symbols,
                    "diagnostics": diags
                }))
            }
            "workspace_analysis" => {
                let projects = self.get_project(id).await?;
                Ok(json!({
                    "project": projects,
                    "hint": "Use workspace_symbols / file_analysis / call_graph for deeper views"
                }))
            }
            other => Err(ServiceError::InvalidRequest(format!(
                "Неизвестный analysis_type: {other}. Допустимо: workspace_symbols, file_analysis, workspace_analysis"
            ))),
        }
    }

    pub async fn did_change_watched_files(
        &self,
        id: &str,
        changes: Vec<Value>,
    ) -> Result<Value, ServiceError> {
        let project = self.project_handle(id).await?;
        invalidate_source_cache(&project, "did_change").await;
        let client = {
            let state = project.read().await;
            state.client.clone().ok_or_else(|| {
                ServiceError::ProjectNotReady("Нет активного LSP-клиента".to_string())
            })?
        };
        client
            .notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": changes }),
            )
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        // Drop cached open state so the next tool call re-syncs from disk.
        // Otherwise we may skip didChange while LS already reloaded via watcher.
        {
            let mut state = project.write().await;
            state.opened_files.clear();
        }
        Ok(json!({ "ok": true, "changes": changes.len() }))
    }

    pub async fn workspace_symbols(&self, id: &str, query: &str) -> Result<Value, ServiceError> {
        let project = self.project_handle(id).await?;
        let (client, ready) = {
            let state = project.read().await;
            if !state.status.is_running() {
                return Err(ServiceError::ProjectNotReady(format!(
                    "Проект еще не готов. Текущий статус: {}.",
                    project_status_label(&state.status)
                )));
            }
            let client = state.client.clone().ok_or_else(|| {
                ServiceError::ProjectNotReady("У проекта нет активного LSP-клиента.".to_string())
            })?;
            (client, state.status.is_ready())
        };

        let this = self.clone();
        let id = id.to_string();
        let query_owned = query.to_string();
        let key = CacheKey::new("workspace_symbols", "", None, None, query, 0);
        query_cached(&project, ready, key, || {
            let this = this.clone();
            let id = id.clone();
            let query_owned = query_owned.clone();
            let client = client.clone();
            async move {
                this.log(&id, format!(">> workspace/symbol: \"{query_owned}\""))
                    .await;

                let result = client
                    .request("workspace/symbol", json!({ "query": query_owned }))
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()));

                match &result {
                    Ok(value) => {
                        let count = value.as_array().map(|a| a.len()).unwrap_or(0);
                        this.log(&id, format!("<< workspace/symbol: {count} символов"))
                            .await;
                    }
                    Err(err) => {
                        this.log(&id, format!("<< workspace/symbol ошибка: {err}"))
                            .await;
                    }
                }

                result
            }
        })
        .await
    }

    pub async fn project_logs(&self, id: &str, tail: usize) -> Result<Vec<String>, ServiceError> {
        self.project_handle(id).await?;
        tail_project_log(self.paths.logs_dir.clone(), id.to_string(), tail)
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))
    }

    pub async fn clear_project_logs(&self, id: &str) -> Result<(), ServiceError> {
        self.project_handle(id).await?;
        clear_project_logs(self.paths.logs_dir.clone(), id.to_string())
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))
    }

    /// BSL LS often ignores `$/cancelRequest` while stuck in completion/signatureHelp.
    /// Kill the Java process and restart so the request queue is not permanently wedged.
    /// Avoids `shutdown` RPC (would deadlock on the in-flight semaphore held by drain).
    async fn kick_lsp_after_hang(&self, id: &str, reason: &str) {
        tracing::warn!("LSP hang after {reason} on project {id}; killing and restarting LS");
        let _ = append_project_log(
            self.paths.logs_dir.clone(),
            id.to_string(),
            format!("!! LSP hang ({reason}) — kill+restart language server"),
        )
        .await;

        let Ok(project) = self.project_handle(id).await else {
            return;
        };
        let child = {
            let mut state = project.write().await;
            detach_after_hang(&mut state)
        };
        self.emit_status(id, &project).await;

        if let Some(child) = child {
            let _ = child.lock().await.kill().await;
            let _ = child.lock().await.wait().await;
        }

        let _ = self.start_project(id).await;
    }

    async fn with_file_cache<F, Fut>(
        &self,
        ctx: &OpenedFileContext,
        tool: &'static str,
        line: Option<u32>,
        character: Option<u32>,
        params: &str,
        compute: F,
    ) -> Result<Value, ServiceError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<Value, ServiceError>>,
    {
        let key = CacheKey::new(
            tool,
            &ctx.file_path,
            line,
            character,
            params,
            ctx.content_hash,
        );
        query_cached(&ctx.project, ctx.status.is_ready(), key, compute).await
    }

    async fn ensure_file_opened(
        &self,
        id: &str,
        file_path: &str,
    ) -> Result<OpenedFileContext, ServiceError> {
        let project = self.project_handle(id).await?;
        let (root_path, project_root_path, client, ready, prev) = {
            let state = project.read().await;
            (
                state.config.root_path.clone(),
                state.config.project_root_path.clone(),
                state.client.clone(),
                state.status.clone(),
                state.opened_files.get(file_path).copied(),
            )
        };

        if !ready.is_running() {
            return Err(ServiceError::ProjectNotReady(format!(
                "Проект еще не готов. Текущий статус: {}.",
                project_status_label(&ready)
            )));
        }

        let client = client.ok_or_else(|| {
            ServiceError::ProjectNotReady("У проекта нет активного LSP-клиента.".to_string())
        })?;

        let absolute_path = resolve_project_file(
            Path::new(&root_path),
            Path::new(&project_root_path),
            file_path,
        )
        .await?;
        let uri = Url::from_file_path(&absolute_path)
            .map_err(|_| ServiceError::InvalidRequest("Некорректный путь к файлу.".to_string()))?
            .to_string();

        let content = tokio::fs::read_to_string(&absolute_path)
            .await
            .map_err(|err| ServiceError::FileNotFound(err.to_string()))?;
        let content_hash = hash_content(&content);

        match prev {
            None => {
                // First time — send didOpen
                client
                    .notify(
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": "bsl",
                                "version": 1,
                                "text": content,
                            }
                        }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()))?;

                project.write().await.opened_files.insert(
                    file_path.to_string(),
                    OpenedDoc {
                        version: 1,
                        content_hash,
                    },
                );
            }
            Some(doc) if doc.content_hash == content_hash => {
                // Same content already synced with LS — skip didChange.
                // This is critical: completion/signatureHelp used to re-send the whole
                // module every call and stall the single-threaded BSL LS for 10–15s.
            }
            Some(doc) => {
                let new_version = doc.version.saturating_add(1);
                {
                    let mut state = project.write().await;
                    state.diagnostics.remove(&uri);
                    state.opened_files.insert(
                        file_path.to_string(),
                        OpenedDoc {
                            version: new_version,
                            content_hash,
                        },
                    );
                }
                client
                    .notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "version": new_version,
                            },
                            "contentChanges": [{ "text": content }],
                        }),
                    )
                    .await
                    .map_err(|err| ServiceError::Internal(err.to_string()))?;
            }
        }

        let status = project.read().await.status.clone();
        Ok(OpenedFileContext {
            project,
            uri,
            client,
            content_hash,
            status,
            file_path: file_path.to_string(),
        })
    }

    async fn log(&self, id: &str, line: String) {
        let is_debug = match self.projects.read().await.get(id) {
            Some(p) => p.read().await.config.debug,
            None => false,
        };
        if !is_debug {
            return;
        }
        let _ = append_project_log(self.paths.logs_dir.clone(), id.to_string(), line.clone()).await;
        let _ = self.event_tx.send(LspEvent::LogLine {
            id: id.to_string(),
            line,
        });
    }

    async fn project_handle(&self, id: &str) -> Result<SharedProject, ServiceError> {
        self.projects
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| ServiceError::NotFound(format!("Проект с id {id} не найден.")))
    }

    async fn emit_status(&self, id: &str, project: &SharedProject) {
        let status = project.read().await.status.info();
        let _ = self.event_tx.send(LspEvent::ProjectStatusChanged {
            id: id.to_string(),
            status,
        });
    }

    async fn set_error(&self, id: &str, project: &SharedProject, message: &str) {
        {
            let mut state = project.write().await;
            state.status = ProjectStatus::Error(message.to_string());
            state.startup_cancel = None;
            state.client = None;
            state.child = None;
        }
        self.emit_status(id, project).await;
    }

    async fn finish_user_stop(
        &self,
        id: &str,
        project: &SharedProject,
    ) -> Result<ProjectSnapshot, ServiceError> {
        {
            let mut state = project.write().await;
            state.status = ProjectStatus::Stopped;
            state.startup_cancel = None;
            state.client = None;
            state.child = None;
            state.watcher = None;
            state.server_caps = None;
            state.progress = IndexingProgress::default();
        }
        self.emit_status(id, project).await;
        Err(ServiceError::InvalidRequest(
            "Запуск остановлен пользователем.".to_string(),
        ))
    }
}

fn reset_response_cache(state: &ProjectState, reason: &'static str) {
    state.response_cache.clear(reason);
}

fn begin_start(
    state: &mut ProjectState,
    cancel_tx: watch::Sender<bool>,
) -> Result<(String, String, String, String), ServiceError> {
    if !state.status.is_stopped() {
        return Err(ServiceError::InvalidRequest(
            "Проект уже запущен.".to_string(),
        ));
    }

    state.status = ProjectStatus::Starting;
    state.progress = IndexingProgress::default();
    state.diagnostics.clear();
    state.opened_files.clear();
    state.server_caps = None;
    state.client = None;
    state.child = None;
    state.startup_cancel = Some(cancel_tx);
    reset_response_cache(state, "start");

    Ok((
        state.config.root_path.clone(),
        state.config.jvm_args.clone(),
        state.config.bsl_config.clone(),
        state.config.id.clone(),
    ))
}

fn apply_stop(state: &mut ProjectState) {
    state.status = ProjectStatus::Stopped;
    state.startup_cancel = None;
    state.client = None;
    state.child = None;
    state.watcher = None;
    state.server_caps = None;
    state.diagnostics.clear();
    state.opened_files.clear();
    state.progress = IndexingProgress::default();
    reset_response_cache(state, "stop");
}

fn detach_after_hang(state: &mut ProjectState) -> Option<Arc<Mutex<Child>>> {
    state.watcher = None;
    state.client = None;
    state.server_caps = None;
    state.opened_files.clear();
    state.diagnostics.clear();
    state.status = ProjectStatus::Stopped;
    reset_response_cache(state, "hang");
    state.child.take()
}

enum LspProcessOutcome {
    Success,
    Failure(String),
}

fn apply_process_exit(
    state: &mut ProjectState,
    outcome: LspProcessOutcome,
) -> Option<crate::models::ProjectStatusInfo> {
    state.client = None;
    state.child = None;
    state.watcher = None;
    state.server_caps = None;
    state.opened_files.clear();
    reset_response_cache(state, "process_exit");
    if state.status.is_stopped() {
        None
    } else {
        state.status = match outcome {
            LspProcessOutcome::Success => ProjectStatus::Stopped,
            LspProcessOutcome::Failure(message) => ProjectStatus::Error(message),
        };
        Some(state.status.info())
    }
}

fn promote_to_ready(state: &mut ProjectState) -> bool {
    if !matches!(state.status, ProjectStatus::WarmingUp) {
        return false;
    }
    state.status = ProjectStatus::Ready;
    reset_response_cache(state, "ready");
    true
}

pub(crate) async fn invalidate_source_cache(project: &SharedProject, reason: &'static str) {
    project.read().await.response_cache.clear(reason);
}

async fn query_cached<F, Fut>(
    project: &SharedProject,
    ready: bool,
    key: CacheKey,
    compute: F,
) -> Result<Value, ServiceError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Value, ServiceError>>,
{
    let cache = project.read().await.response_cache.clone();
    if !ready {
        cache.note_skip_not_ready(key.tool);
        return compute().await;
    }
    cache.get_or_compute(key, compute).await
}

fn startup_was_cancelled(state: &ProjectState) -> bool {
    state.status.is_stopped() || state.startup_cancel.as_ref().is_some_and(|tx| *tx.borrow())
}

async fn kill_child(child: &Arc<Mutex<Child>>) {
    let mut child = child.lock().await;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn lsp_diagnostic_items(payload: &Value) -> Vec<Value> {
    payload
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn hash_content(content: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn validate_name(name: &str) -> Result<String, ServiceError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "Имя проекта не должно быть пустым.".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn canonical_root_path(path: &str) -> Result<String, ServiceError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "Корневой путь не указан.".to_string(),
        ));
    }

    let canonical =
        clean_path(PathBuf::from(path).canonicalize().map_err(|_| {
            ServiceError::InvalidRequest(format!("Корневой путь не найден: {path}"))
        })?);
    if !canonical.is_dir() {
        return Err(ServiceError::InvalidRequest(format!(
            "Корневой путь не является каталогом: {path}"
        )));
    }
    Ok(canonical.to_string_lossy().to_string())
}

/// Разрешает относительный путь к файлу в абсолютный.
///
/// `project_root` — корень всего проекта, откуда работают LLM-агенты и CLI.
/// Путь разрешается относительно `project_root`, но итоговый абсолютный путь
/// должен находиться внутри `root_path` (корня BSL).
async fn resolve_project_file(
    root_path: &Path,
    project_root: &Path,
    relative_path: &str,
) -> Result<PathBuf, ServiceError> {
    let relative = PathBuf::from(relative_path);
    if relative.is_absolute() {
        return Err(ServiceError::InvalidRequest(
            "Путь к файлу должен быть относительным относительно корня проекта.".to_string(),
        ));
    }

    let root = clean_path(tokio::fs::canonicalize(root_path).await.map_err(|_| {
        ServiceError::InvalidRequest("Корневой каталог BSL недоступен.".to_string())
    })?);

    let resolve_base = clean_path(tokio::fs::canonicalize(project_root).await.map_err(|_| {
        ServiceError::InvalidRequest("Корневой каталог проекта недоступен.".to_string())
    })?);

    let absolute = clean_path(
        tokio::fs::canonicalize(resolve_base.join(&relative))
            .await
            .map_err(|_| ServiceError::FileNotFound(relative_path.to_string()))?,
    );

    if !absolute.starts_with(&root) {
        return Err(ServiceError::InvalidRequest(
            "Путь к файлу выходит за пределы корня BSL.".to_string(),
        ));
    }

    Ok(absolute)
}

/// Убирает Windows extended-length prefix `\\?\` из пути.
/// `canonicalize()` на Windows добавляет этот префикс, что ломает
/// повторную обработку пути и выглядит некорректно в UI.
fn clean_path(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

fn project_notification_handler(
    project: SharedProject,
    logs_dir: PathBuf,
    event_tx: broadcast::Sender<LspEvent>,
    project_id: String,
) -> NotificationHandler {
    Arc::new(move |method, params| {
        let project = project.clone();
        let logs_dir = logs_dir.clone();
        let event_tx = event_tx.clone();
        let project_id = project_id.clone();
        tokio::spawn(async move {
            let debug = project.read().await.config.debug;

            if debug {
                let line = format!("[LSP] {method}: {params}");
                let _ =
                    append_project_log(logs_dir.clone(), project_id.clone(), line.clone()).await;
                let _ = event_tx.send(LspEvent::LogLine {
                    id: project_id.clone(),
                    line,
                });
            }

            match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    if let Some(raw_uri) = params.get("uri").and_then(|value| value.as_str()) {
                        // Normalize URI: BSL server may send non-encoded Cyrillic,
                        // but ensure_file_opened uses Url::from_file_path which percent-encodes.
                        let uri = Url::parse(raw_uri)
                            .map(|u| u.to_string())
                            .unwrap_or_else(|_| raw_uri.to_string());
                        let count = params
                            .get("diagnostics")
                            .and_then(|value| value.as_array())
                            .map(Vec::len)
                            .unwrap_or(0);
                        project
                            .write()
                            .await
                            .diagnostics
                            .insert(uri.clone(), params.clone());
                        let _ = event_tx.send(LspEvent::DiagnosticsUpdated {
                            id: project_id,
                            file: uri,
                            count,
                        });
                    }
                }
                "$/progress" => {
                    let token = params
                        .get("token")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let value = params.get("value").cloned().unwrap_or(Value::Null);
                    let kind = value
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();

                    let mut state = project.write().await;

                    // Filter by token: only track the main progress
                    // "report" always comes from the main task → adopt its token
                    // "begin" only processed if no main token yet
                    // "end" only processed if from the main token
                    let dominated = match kind {
                        "begin" => state.progress_token.is_some(),
                        "report" => {
                            if state.progress_token.is_none() {
                                state.progress_token = token.clone();
                            }
                            state.progress_token != token
                        }
                        "end" => state.progress_token != token,
                        _ => true,
                    };

                    if !dominated {
                        apply_progress_value(&mut state.progress, &value);

                        if kind == "end" {
                            state.progress_token = None;
                        }

                        let _ = event_tx.send(LspEvent::IndexingProgress {
                            id: project_id.clone(),
                            progress: state.progress.clone(),
                        });

                        let end_message = value
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        let is_context_ready = kind == "end"
                            && (end_message.contains("Наполнение контекста завершено")
                                || end_message.contains("Context populated"));

                        if is_context_ready && promote_to_ready(&mut state) {
                            let _ = event_tx.send(LspEvent::ProjectStatusChanged {
                                id: project_id,
                                status: state.status.info(),
                            });
                        }
                    }
                }
                _ => {}
            }
        });
    })
}

fn project_stderr_handler(
    project: SharedProject,
    logs_dir: PathBuf,
    event_tx: broadcast::Sender<LspEvent>,
    project_id: String,
) -> StderrHandler {
    Arc::new(move |line| {
        let project = project.clone();
        let logs_dir = logs_dir.clone();
        let event_tx = event_tx.clone();
        let project_id = project_id.clone();
        tokio::spawn(async move {
            let _ = append_project_log(logs_dir, project_id.clone(), line.clone()).await;
            let _ = event_tx.send(LspEvent::LogLine {
                id: project_id.clone(),
                line: line.clone(),
            });

            if line.contains("Наполнение контекста завершено") || line.contains("Context populated")
            {
                let mut state = project.write().await;
                if promote_to_ready(&mut state) {
                    let _ = event_tx.send(LspEvent::ProjectStatusChanged {
                        id: project_id,
                        status: state.status.info(),
                    });
                }
            }
        });
    })
}

fn apply_progress_value(progress: &mut IndexingProgress, value: &Value) {
    let kind = value
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    match kind {
        "begin" => {
            progress.active = true;
            progress.percentage = value
                .get("percentage")
                .and_then(|value| value.as_u64())
                .map(|value| value as u32);
            progress.message = value
                .get("title")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
        }
        "report" => {
            progress.active = true;
            if let Some(percentage) = value.get("percentage").and_then(|value| value.as_u64()) {
                progress.percentage = Some(percentage as u32);
            }
            if let Some(message) = value.get("message").and_then(|value| value.as_str()) {
                progress.message = Some(message.to_string());
            }
        }
        "end" => {
            progress.active = false;
            progress.message = value
                .get("message")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
            progress.percentage = Some(100);
        }
        _ => {}
    }
}

async fn initialize_client(
    client: &LspClient,
    root_path: &str,
    cancel: watch::Receiver<bool>,
) -> AnyResult<ServerCaps> {
    let root_uri = Url::from_directory_path(root_path)
        .map_err(|_| anyhow::anyhow!("invalid root path"))?
        .to_string();

    let result = client
        .request_until_cancel(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": "project"
                }],
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": true },
                        "diagnostic": { "dynamicRegistration": false },
                        "synchronization": { "didSave": true, "didChange": true },
                        "hover": {
                            "dynamicRegistration": false,
                            "contentFormat": ["markdown", "plaintext"]
                        },
                        "completion": {
                            "dynamicRegistration": false,
                            "completionItem": {
                                "snippetSupport": false,
                                "documentationFormat": ["markdown", "plaintext"],
                                "labelDetailsSupport": true
                            },
                            "contextSupport": true
                        },
                        "signatureHelp": {
                            "dynamicRegistration": false,
                            "signatureInformation": {
                                "documentationFormat": ["markdown", "plaintext"],
                                "parameterInformation": { "labelOffsetSupport": true }
                            },
                            "contextSupport": true
                        },
                        "codeAction": {
                            "dynamicRegistration": false
                        },
                        "rename": {
                            "dynamicRegistration": false,
                            "prepareSupport": true
                        },
                        "documentSymbol": {
                            "hierarchicalDocumentSymbolSupport": true
                        },
                        "references": {
                            "dynamicRegistration": false
                        },
                        "definition": {
                            "dynamicRegistration": false,
                            "linkSupport": true
                        },
                        "callHierarchy": {
                            "dynamicRegistration": false
                        },
                        "inlayHint": {
                            "dynamicRegistration": false
                        },
                        "selectionRange": {
                            "dynamicRegistration": false
                        },
                        "semanticTokens": {
                            "dynamicRegistration": false,
                            "requests": { "range": true },
                            "formats": ["relative"]
                        }
                    },
                    "window": {
                        "workDoneProgress": true
                    },
                    "workspace": {
                        "symbol": {
                            "dynamicRegistration": false
                        },
                        "didChangeWatchedFiles": {
                            "dynamicRegistration": false
                        }
                    }
                }
            }),
            cancel,
        )
        .await?;

    let capabilities = result
        .get("capabilities")
        .or_else(|| {
            result
                .get("result")
                .and_then(|value| value.get("capabilities"))
        })
        .cloned()
        .unwrap_or(Value::Null);

    let server_caps = ServerCaps {
        supports_pull_diagnostics: capability_supported(capabilities.get("diagnosticProvider")),
        supports_call_hierarchy: capability_supported(capabilities.get("callHierarchyProvider")),
        supports_did_save: text_document_sync_supports_save(capabilities.get("textDocumentSync")),
    };

    client.notify("initialized", json!({})).await?;
    Ok(server_caps)
}

async fn watch_project_process(
    project: SharedProject,
    event_tx: broadcast::Sender<LspEvent>,
    project_id: String,
    child: Arc<Mutex<Child>>,
) {
    loop {
        let exit_status = {
            let mut child = child.lock().await;
            match child.try_wait() {
                Ok(status) => status,
                Err(err) => {
                    tracing::error!("failed to inspect child process: {err}");
                    None
                }
            }
        };

        if let Some(status) = exit_status {
            let outcome = if status.success() {
                LspProcessOutcome::Success
            } else {
                LspProcessOutcome::Failure(format!("LSP-процесс завершился с ошибкой: {status}"))
            };
            let maybe_status = {
                let mut state = project.write().await;
                apply_process_exit(&mut state, outcome)
            };

            if let Some(status) = maybe_status {
                let _ = event_tx.send(LspEvent::ProjectStatusChanged {
                    id: project_id,
                    status,
                });
            }
            break;
        }

        sleep(Duration::from_millis(500)).await;
    }
}

fn map_create_project_error(err: &anyhow::Error) -> ServiceError {
    map_project_persist_error(err, "Не удалось создать проект.")
}

fn map_update_project_error(err: &anyhow::Error) -> ServiceError {
    map_project_persist_error(err, "Не удалось обновить проект.")
}

fn map_project_persist_error(err: &anyhow::Error, fallback: &str) -> ServiceError {
    let text = err.to_string();
    if text.contains("UNIQUE constraint failed: projects.root_path") {
        return ServiceError::InvalidRequest(
            "Проект с таким корневым путем уже существует.".to_string(),
        );
    }

    let reason = err
        .chain()
        .last()
        .map(|cause| cause.to_string())
        .unwrap_or(text);
    let reason = normalize_project_persist_reason(&reason);
    ServiceError::InvalidRequest(format!("{fallback} Причина: {reason}"))
}

fn project_status_label(status: &ProjectStatus) -> &'static str {
    match status {
        ProjectStatus::Stopped => "остановлен",
        ProjectStatus::Starting => "запускается",
        ProjectStatus::WarmingUp => "прогревается",
        ProjectStatus::Ready => "готов",
        ProjectStatus::Error(_) => "ошибка",
    }
}

fn normalize_project_persist_reason(reason: &str) -> String {
    let reason = reason
        .trim()
        .trim_start_matches("error returned from database:")
        .trim()
        .trim_start_matches("failed to insert project:")
        .trim_start_matches("failed to update project:")
        .trim();

    if reason.contains("database is locked") {
        return "база данных занята другим процессом".to_string();
    }
    if reason.contains("unable to open database file") {
        return "не удалось открыть файл базы данных".to_string();
    }
    if reason.contains("disk I/O error") {
        return "ошибка ввода-вывода при работе с базой данных".to_string();
    }
    if reason.is_empty() {
        return "неизвестная ошибка".to_string();
    }

    reason.to_string()
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

const DEFAULT_BSL_CONFIG: &str = r#"{
  "$schema": "https://1c-syntax.github.io/bsl-language-server/configuration/schema.json",
  "language": "ru",
  "diagnostics": {
    "minimumLSPDiagnosticLevel": "Error",
    "parameters": {
      "MethodSize": false
    }
  }
}"#;

/// Returns the effective BSL config: user-provided or default.
/// If the user config is empty, returns the default config.
fn effective_bsl_config(user_config: &str) -> &str {
    if user_config.trim().is_empty() {
        DEFAULT_BSL_CONFIG
    } else {
        user_config
    }
}

/// Validates BSL config JSON. Empty config is OK (default is used at start).
fn validate_bsl_config(config: &str) -> Result<(), ServiceError> {
    if config.trim().is_empty() {
        return Ok(());
    }
    serde_json::from_str::<Value>(config.trim()).map_err(|err| {
        ServiceError::InvalidRequest(format!("Конфигурация BSL содержит невалидный JSON: {err}"))
    })?;
    Ok(())
}

fn capability_supported(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(supported)) => *supported,
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn text_document_sync_supports_save(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };

    match value {
        Value::Number(_) => false,
        Value::Object(sync) => match sync.get("save") {
            Some(Value::Bool(supported)) => *supported,
            Some(Value::Null) | None => false,
            Some(_) => true,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::{watch, RwLock};

    use super::*;
    use crate::config::AppConfig;
    use crate::db::Database;
    use crate::models::ProjectUpsert;
    use crate::paths::AppPaths;

    fn project_with(status: ProjectStatus) -> SharedProject {
        let mut state = ProjectState::from_config(StoredProject {
            id: "p1".to_string(),
            name: "test".to_string(),
            root_path: "root".to_string(),
            project_root_path: "root".to_string(),
            jvm_args: String::new(),
            bsl_config: String::new(),
            debug: false,
            created_at: String::new(),
            updated_at: String::new(),
        });
        state.status = status;
        Arc::new(RwLock::new(state))
    }

    async fn seed_cache(project: &SharedProject) {
        let cache = project.read().await.response_cache.clone();
        cache
            .get_or_compute(
                CacheKey::new("diagnostics", "src/mod.bsl", None, None, "", 1),
                || async { Ok(json!({"ok": true})) },
            )
            .await
            .unwrap();
    }

    async fn assert_cache_empty(project: &SharedProject) {
        let cache = project.read().await.response_cache.clone();
        assert_eq!(cache.ready_len(), 0);
        assert_eq!(cache.inflight_len(), 0);
    }

    async fn empty_manager(dir: &Path) -> LspManager {
        let paths = AppPaths {
            config_dir: dir.join("config"),
            data_local_dir: dir.join("data"),
            state_dir: dir.join("state"),
            logs_dir: dir.join("state").join("logs"),
            bsl_configs_dir: dir.join("data").join("bsl-configs"),
            config_path: dir.join("config").join("config.toml"),
            db_path: dir.join("data").join("data.db"),
            runtime_path: dir.join("state").join("runtime.json"),
        };
        let db = Database::open(&paths.db_path).await.unwrap();
        LspManager::load(paths, Arc::new(RwLock::new(AppConfig::default())), db)
            .await
            .unwrap()
    }

    #[test]
    fn quality_diagnostics_reads_diagnostics_field() {
        let payload = json!({
            "uri": "file:///Module.bsl",
            "diagnostics": [{
                "code": "AssignAliasFieldsInQuery",
                "message": "alias",
                "severity": 2,
                "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
            }]
        });
        assert!(payload.as_array().is_none());
        let items = lsp_diagnostic_items(&payload);
        assert_eq!(items.len(), 1);
        let filtered = crate::analysis::filter_quality_diagnostics(
            &items,
            &["security", "performance", "sql"],
        );
        assert_eq!(filtered["count"], 1);
        let missed = crate::analysis::filter_quality_diagnostics(&[], &["sql"]);
        assert_eq!(missed["count"], 0);
    }

    #[tokio::test]
    async fn warming_up_result_is_not_cached() {
        let project = project_with(ProjectStatus::WarmingUp);
        let calls = AtomicUsize::new(0);
        let key = CacheKey::new("symbols", "a.bsl", None, None, "", 1);
        for _ in 0..2 {
            query_cached(&project, false, key.clone(), || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!(1)) }
            })
            .await
            .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(project.read().await.response_cache.ready_len(), 0);
        assert_eq!(
            project.read().await.response_cache.stats().skip_not_ready,
            2
        );

        {
            let mut state = project.write().await;
            state.status = ProjectStatus::Ready;
        }
        query_cached(&project, true, key.clone(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(json!(1)) }
        })
        .await
        .unwrap();
        let hit = query_cached(&project, true, key, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(json!(2)) }
        })
        .await
        .unwrap();
        assert_eq!(hit, json!(1));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(project.read().await.response_cache.ready_len(), 1);
    }

    #[tokio::test]
    async fn lifecycle_transitions_clear_cache() {
        let project = project_with(ProjectStatus::Stopped);
        seed_cache(&project).await;
        {
            let mut state = project.write().await;
            let (tx, _rx) = watch::channel(false);
            begin_start(&mut state, tx).unwrap();
        }
        assert_cache_empty(&project).await;

        {
            let mut state = project.write().await;
            state.status = ProjectStatus::Ready;
        }
        seed_cache(&project).await;
        {
            let mut state = project.write().await;
            apply_stop(&mut state);
        }
        assert_cache_empty(&project).await;

        {
            let mut state = project.write().await;
            state.status = ProjectStatus::Ready;
        }
        seed_cache(&project).await;
        {
            let mut state = project.write().await;
            assert!(detach_after_hang(&mut state).is_none());
        }
        assert_cache_empty(&project).await;

        {
            let mut state = project.write().await;
            state.status = ProjectStatus::Ready;
        }
        seed_cache(&project).await;
        {
            let mut state = project.write().await;
            let info = apply_process_exit(
                &mut state,
                LspProcessOutcome::Failure("lsp exited".to_string()),
            );
            assert!(info.is_some());
        }
        assert_cache_empty(&project).await;

        {
            let mut state = project.write().await;
            state.status = ProjectStatus::WarmingUp;
        }
        seed_cache(&project).await;
        {
            let mut state = project.write().await;
            assert!(promote_to_ready(&mut state));
        }
        assert_cache_empty(&project).await;
        assert!(project.read().await.status.is_ready());

        seed_cache(&project).await;
        {
            let mut state = project.write().await;
            assert!(!promote_to_ready(&mut state));
        }
        assert_eq!(project.read().await.response_cache.ready_len(), 1);
    }

    #[tokio::test]
    async fn watcher_invalidation_and_did_change_clear_cache() {
        let project = project_with(ProjectStatus::Ready);
        seed_cache(&project).await;
        invalidate_source_cache(&project, "watcher").await;
        assert_cache_empty(&project).await;

        let dir = tempfile::tempdir().unwrap();
        let manager = empty_manager(dir.path()).await;
        let root = dir.path().join("cfg");
        std::fs::create_dir_all(&root).unwrap();
        let root_path = root.to_string_lossy().to_string();
        let snapshot = manager
            .create_project(ProjectUpsert {
                name: "cache".to_string(),
                root_path: root_path.clone(),
                project_root_path: root_path,
                jvm_args: String::new(),
                bsl_config: String::new(),
                debug: false,
            })
            .await
            .unwrap();
        let id = snapshot.project.id.clone();
        let handle = manager.projects.read().await.get(&id).unwrap().clone();
        seed_cache(&handle).await;
        let error = manager
            .did_change_watched_files(&id, Vec::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("LSP"));
        assert_cache_empty(&handle).await;
    }

    #[tokio::test]
    async fn module_health_reuses_primary_caches_without_its_own_entry() {
        let project = project_with(ProjectStatus::Ready);
        let complexity_calls = AtomicUsize::new(0);
        let diagnostic_calls = AtomicUsize::new(0);
        let complexity_key = CacheKey::new("complexity", "a.bsl", None, None, "", 9);
        let diagnostics_key = CacheKey::new("diagnostics", "a.bsl", None, None, "", 9);

        for _ in 0..2 {
            query_cached(&project, true, complexity_key.clone(), || {
                complexity_calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({"flagged": []})) }
            })
            .await
            .unwrap();
            let diags = query_cached(&project, true, diagnostics_key.clone(), || {
                diagnostic_calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Ok(json!({
                        "uri": "file:///a.bsl",
                        "diagnostics": [{
                            "code": "AssignAliasFieldsInQuery",
                            "message": "alias",
                            "severity": 2,
                            "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
                        }]
                    }))
                }
            })
            .await
            .unwrap();
            let quality = crate::analysis::filter_quality_diagnostics(
                &lsp_diagnostic_items(&diags),
                &["sql"],
            );
            let _health = crate::analysis::merge_module_health(&json!({"flagged": []}), &quality);
            assert_eq!(quality["count"], 1);
        }

        assert_eq!(complexity_calls.load(Ordering::SeqCst), 1);
        assert_eq!(diagnostic_calls.load(Ordering::SeqCst), 1);
        let names = project.read().await.response_cache.tool_names();
        assert!(names.contains(&"complexity"));
        assert!(names.contains(&"diagnostics"));
        assert!(!names.contains(&"quality_diagnostics"));
        assert!(!names.contains(&"module_health"));
        assert_eq!(project.read().await.response_cache.ready_len(), 2);
    }
}
