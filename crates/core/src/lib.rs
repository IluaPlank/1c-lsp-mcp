pub mod analysis;
pub mod bsl_context;
mod cache;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod java;
pub mod logging;
pub mod lsp;
pub mod manager;
pub mod models;
pub mod page;
pub mod paths;
pub mod runtime;
pub mod toon;
pub mod watcher;

pub use bsl_context::{
    spawn_bsl_context, tool_result_text, BslContextClient, BslContextService, PLATFORM_TOOLS,
};
pub use config::AppConfig;
pub use db::Database;
pub use error::{ErrorResponse, ServiceError};
pub use events::LspEvent;
pub use java::check_java;
pub use manager::LspManager;
pub use models::{
    IndexingProgress, JavaCheckResult, ProjectSnapshot, ProjectStatus, ProjectStatusInfo,
    ProjectUpsert, StoredProject,
};
pub use paths::AppPaths;
pub use runtime::{compute_connect_url, RuntimeMetadata};
