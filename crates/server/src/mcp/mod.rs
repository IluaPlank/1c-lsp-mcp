mod protocol;

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use lsp_skill_core::{tool_result_text, BslContextService, LspManager, PLATFORM_TOOLS};

use protocol::*;

macro_rules! toon_note {
    () => {
        " Формат ответа зависит от настройки use_toon_format. По умолчанию true: \
         компактный TOON, его и стоит читать агенту. При false возвращается \
         стандартный LSP JSON. В TOON: range -> range_sl/range_sc/range_el/range_ec; selectionRange -> \
         selection_range_sl/selection_range_sc/selection_range_el/selection_range_ec; \
         location -> location_uri/location_sl/location_sc/location_el/location_ec; \
         targetUri -> target_uri; targetRange -> target_range_sl/target_range_sc/target_range_el/target_range_ec; \
         targetSelectionRange -> target_selection_range_sl/target_selection_range_sc/target_selection_range_el/target_selection_range_ec; \
         originSelectionRange -> origin_selection_range_sl/origin_selection_range_sc/origin_selection_range_el/origin_selection_range_ec \
         (null если отсутствует); containerName -> container_name; from/to инлайнятся с \
         префиксом from_/to_. Координаты 0-based."
    };
}

macro_rules! page_note {
    () => {
        " По умолчанию сводка: total и первая страница items (до 20). \
         В items сами элементы ответа, не идентификатор. Следующая страница: \
         offset=next_offset. detail=full — страница до 200 без группировки по файлам. \
         limit от 1 до 200."
    };
}

// ── State & types ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct McpState {
    manager: Arc<LspManager>,
    bsl_context: Arc<BslContextService>,
    server_name: &'static str,
    server_instructions: &'static str,
    tools: &'static [ToolDef],
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,
}

const SERVER_NAME: &str = "1c";
const SERVER_INSTRUCTIONS: &str =
    "Единый MCP для 1С:Предприятие 8 (BSL): диагностика, навигация, hover/completion, \
     rename/code_actions, справка API платформы (bsl-context), статус проекта. \
     Использует bsl-language-server. Передавайте file_path относительно корня проекта \
     (project_root_path). Заголовок x-project-id обязателен для code tools. \
     Platform tools (search/info/getMembers/…) не требуют x-project-id. \
     Координаты line/character — zero-based. \
     references, workspace_symbols и call_graph по умолчанию отдают сводку и страницу items; \
     следующая страница — offset=next_offset, дерево графа и крупная страница — detail=full. \
     В ответе сами элементы, не идентификатор кэша.";

// ── Router ───────────────────────────────────────────────────────────────────

pub fn router(manager: Arc<LspManager>, bsl_context: Arc<BslContextService>) -> Router {
    let state = McpState {
        manager,
        bsl_context,
        server_name: SERVER_NAME,
        server_instructions: SERVER_INSTRUCTIONS,
        tools: UNIFIED_TOOLS,
    };

    Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .with_state(state)
}

// ── Tool definitions ─────────────────────────────────────────────────────────

static DIAGNOSTICS_TOOLS: &[ToolDef] = &[ToolDef {
    name: "diagnostics",
    description: concat!(
        "Выполняет статический анализ файла 1С (BSL) через bsl-language-server \
         и возвращает список диагностик: синтаксические ошибки, предупреждения, \
         замечания анализатора кода. Результат — массив LSP Diagnostic с полями \
         range, severity (1=Error, 2=Warning, 3=Information, 4=Hint), message, source, code. \
         Используйте для проверки кода до и после редактирования, а также для объяснения ошибок. \
         Первый запрос к файлу может занять больше времени (файл открывается в LSP-сессии). \
         Пустой результат не гарантирует отсутствие ошибок — возможно, индексация ещё идёт.",
        toon_note!()
    ),
    input_schema: || {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Путь к .bsl файлу относительно корня проекта (project_root_path). \
                                    Не используйте абсолютные пути. Сохраняйте кириллицу в именах каталогов. \
                                    Пример: 1c-src/Configuration/CommonModules/ОбщийМодуль1/Module.bsl"
                }
            },
            "required": ["file_path"]
        })
    },
}];

static FILE_PATH_DESC: &str = "Путь к .bsl файлу относительно корня проекта (project_root_path). \
    Не используйте абсолютные пути. Сохраняйте кириллицу в именах каталогов. \
    Пример: 1c-src/Configuration/CommonModules/ОбщийМодуль1/Module.bsl";
static LINE_DESC: &str = "Номер строки в файле (zero-based, начиная с 0). Первая строка файла = 0";
static CHARACTER_DESC: &str =
    "Позиция символа в строке (zero-based, начиная с 0). Первый символ строки = 0. \
     Проверяйте точность — ошибка на ±1 приводит к пустому или неверному результату";

fn insert_page_props(schema: &mut Value) {
    let Some(props) = schema
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    props.insert(
        "offset".into(),
        json!({
            "type": "integer",
            "description": "Смещение страницы. Следующая страница — next_offset из ответа."
        }),
    );
    props.insert(
        "limit".into(),
        json!({
            "type": "integer",
            "description": "Размер страницы, 1–200. Сводка по умолчанию 20, detail=full — 200."
        }),
    );
    props.insert(
        "detail".into(),
        json!({
            "type": "string",
            "description": "summary (по умолчанию) или full. Для call_graph full — всё дерево."
        }),
    );
}

fn page_query_from_args(args: &Value) -> Result<lsp_skill_core::page::PageQuery, String> {
    lsp_skill_core::page::PageQuery::parse(
        args.get("offset").and_then(|value| value.as_u64()),
        args.get("limit").and_then(|value| value.as_u64()),
        args.get("detail").and_then(|value| value.as_str()),
    )
}

fn position_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": FILE_PATH_DESC
            },
            "line": {
                "type": "integer",
                "description": LINE_DESC
            },
            "character": {
                "type": "integer",
                "description": CHARACTER_DESC
            }
        },
        "required": ["file_path", "line", "character"]
    })
}

static NAVIGATION_TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "symbols",
        description: concat!(
            "Возвращает структуру модуля 1С (BSL): список процедур, функций, \
             переменных и областей (regions) с их позициями и иерархией. \
             Результат — DocumentSymbol[] (с hierarchy и children) или SymbolInformation[]. \
             Используйте для понимания структуры незнакомого модуля перед редактированием. \
             Работает с общими модулями, модулями объектов, менеджеров, форм и команд.",
            toon_note!()
        ),
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": FILE_PATH_DESC
                    }
                },
                "required": ["file_path"]
            })
        },
    },
    ToolDef {
        name: "definition",
        description: concat!(
            "Находит определение (объявление) символа в указанной позиции файла 1С (BSL). \
             Возвращает Location, Location[] или LocationLink[] с файлом и позицией объявления. \
             Используйте для перехода от вызова процедуры/функции к её реализации, \
             в том числе в другие модули. Если результат null — символ не распознан \
             или позиция указана неточно.",
            toon_note!()
        ),
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": FILE_PATH_DESC
                    },
                    "line": {
                        "type": "integer",
                        "description": LINE_DESC
                    },
                    "character": {
                        "type": "integer",
                        "description": CHARACTER_DESC
                    }
                },
                "required": ["file_path", "line", "character"]
            })
        },
    },
    ToolDef {
        name: "references",
        description: concat!(
            "Находит все места использования (ссылки) символа в указанной позиции \
             по всему проекту 1С. Включает само объявление (includeDeclaration: true). \
             Используйте перед изменением или удалением процедуры/функции. \
             Предпочитайте этот инструмент текстовому поиску (grep).",
            page_note!(),
            toon_note!()
        ),
        input_schema: || {
            let mut schema = position_schema();
            insert_page_props(&mut schema);
            schema
        },
    },
    ToolDef {
        name: "workspace_symbols",
        description: concat!(
            "Ищет символы (процедуры, функции, переменные) по всему проекту 1С \
             по текстовому запросу. Поля элемента: name, kind, containerName, location. \
             Используйте когда знаете имя, но не файл. Предпочитайте точное имя. \
             Пустой запрос на большой конфигурации даёт огромный список — берите страницу.",
            page_note!(),
            toon_note!()
        ),
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Текстовый запрос для поиска символов. Используйте точное имя \
                                        процедуры/функции или его часть. \
                                        Пример: ПолучитьФункциональнуюОпцию"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Смещение страницы. Следующая страница — next_offset из ответа."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Размер страницы, 1–200. Сводка по умолчанию 20, detail=full — 200."
                    },
                    "detail": {
                        "type": "string",
                        "description": "summary (по умолчанию) или full."
                    }
                },
                "required": ["query"]
            })
        },
    },
    ToolDef {
        name: "incoming_calls",
        description: concat!(
            "Находит все места, откуда вызывается процедура или функция в указанной позиции. \
             Сначала подготавливает элемент call hierarchy через prepareCallHierarchy, \
             затем возвращает IncomingCall[] с caller и ranges. Если позиция не указывает \
             на процедуру или функцию, результат будет null.",
            toon_note!()
        ),
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": FILE_PATH_DESC
                    },
                    "line": {
                        "type": "integer",
                        "description": LINE_DESC
                    },
                    "character": {
                        "type": "integer",
                        "description": CHARACTER_DESC
                    }
                },
                "required": ["file_path", "line", "character"]
            })
        },
    },
    ToolDef {
        name: "outgoing_calls",
        description: concat!(
            "Находит все процедуры и функции, которые вызываются из процедуры или функции \
             в указанной позиции. Сначала подготавливает элемент call hierarchy через \
             prepareCallHierarchy, затем возвращает OutgoingCall[] с callee и ranges. \
             Если позиция не указывает на процедуру или функцию, результат будет null.",
            toon_note!()
        ),
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": FILE_PATH_DESC
                    },
                    "line": {
                        "type": "integer",
                        "description": LINE_DESC
                    },
                    "character": {
                        "type": "integer",
                        "description": CHARACTER_DESC
                    }
                },
                "required": ["file_path", "line", "character"]
            })
        },
    },
];

/// Полный набор базовых tools единого MCP (без дублей).
static UNIFIED_TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "diagnostics",
        description: DIAGNOSTICS_TOOLS[0].description,
        input_schema: DIAGNOSTICS_TOOLS[0].input_schema,
    },
    ToolDef {
        name: "status",
        description: "Возвращает состояние проекта 1С: status (ready/warming_up/starting/stopped/error), \
         progress индексации, root_path и project_root_path. Используйте при ошибках других tools \
         или чтобы дождаться готовности индекса.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "hover",
        description: concat!(
            "Подсказка по символу в позиции: сигнатура, тип, документация (textDocument/hover). \
             Используйте чтобы понять символ без перехода к definition.",
            toon_note!()
        ),
        input_schema: position_schema,
    },
    ToolDef {
        name: "completion",
        description: concat!(
            "Автодополнение в позиции курсора (textDocument/completion). \
             Interactive-таймаут ~10с; при таймауте очередь LSP не залипает \
             (используйте workspace_symbols / hover как запасной путь).",
            toon_note!()
        ),
        input_schema: position_schema,
    },
    ToolDef {
        name: "code_actions",
        description: concat!(
            "Quick fixes и рефакторинги в позиции (textDocument/codeAction). \
             Используйте рядом с diagnostics для автоматических исправлений.",
            toon_note!()
        ),
        input_schema: position_schema,
    },
    ToolDef {
        name: "prepare_rename",
        description: concat!(
            "Проверяет, можно ли переименовать символ в позиции, и возвращает rename range \
             (textDocument/prepareRename). Вызывайте перед rename.",
            toon_note!()
        ),
        input_schema: position_schema,
    },
    ToolDef {
        name: "rename",
        description: concat!(
            "Переименование символа по всему проекту (textDocument/rename). \
             Возвращает WorkspaceEdit с правками. По умолчанию только preview — \
             файлы не меняются, пока apply=true не будет явно передан. \
             Сначала вызовите prepare_rename.",
            toon_note!()
        ),
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": FILE_PATH_DESC },
                    "line": { "type": "integer", "description": LINE_DESC },
                    "character": { "type": "integer", "description": CHARACTER_DESC },
                    "new_name": {
                        "type": "string",
                        "description": "Новое имя символа"
                    },
                    "apply": {
                        "type": "boolean",
                        "description": "Если true — применить WorkspaceEdit к файлам на диске. \
                                        По умолчанию false (только preview)."
                    }
                },
                "required": ["file_path", "line", "character", "new_name"]
            })
        },
    },
    ToolDef {
        name: "symbols",
        description: NAVIGATION_TOOLS[0].description,
        input_schema: NAVIGATION_TOOLS[0].input_schema,
    },
    ToolDef {
        name: "definition",
        description: NAVIGATION_TOOLS[1].description,
        input_schema: NAVIGATION_TOOLS[1].input_schema,
    },
    ToolDef {
        name: "references",
        description: NAVIGATION_TOOLS[2].description,
        input_schema: NAVIGATION_TOOLS[2].input_schema,
    },
    ToolDef {
        name: "workspace_symbols",
        description: NAVIGATION_TOOLS[3].description,
        input_schema: NAVIGATION_TOOLS[3].input_schema,
    },
    ToolDef {
        name: "incoming_calls",
        description: NAVIGATION_TOOLS[4].description,
        input_schema: NAVIGATION_TOOLS[4].input_schema,
    },
    ToolDef {
        name: "outgoing_calls",
        description: NAVIGATION_TOOLS[5].description,
        input_schema: NAVIGATION_TOOLS[5].input_schema,
    },
    ToolDef {
        name: "search",
        description: "Поиск по API платформы 1С (bsl-context). query — термин 1С; type — method|property|type|null; limit — до 50.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Поисковый запрос (СтрНайти, Справочник, …)" },
                    "type": {
                        "type": ["string", "null"],
                        "description": "method | property | type | null"
                    },
                    "limit": { "type": "integer", "description": "Макс. результатов (1–50)" }
                },
                "required": ["query", "type", "limit"]
            })
        },
    },
    ToolDef {
        name: "info",
        description: "Детальная информация об элементе API платформы 1С по точному имени (bsl-context).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Точное имя элемента API" },
                    "type": {
                        "type": "string",
                        "description": "method | property | type"
                    }
                },
                "required": ["name", "type"]
            })
        },
    },
    ToolDef {
        name: "getMembers",
        description: "Полный список методов и свойств типа 1С (bsl-context).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "typeName": { "type": "string", "description": "Имя типа, напр. ТаблицаЗначений" }
                },
                "required": ["typeName"]
            })
        },
    },
    ToolDef {
        name: "getMember",
        description: "Информация о методе или свойстве конкретного типа 1С (bsl-context).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "typeName": { "type": "string" },
                    "memberName": { "type": "string" }
                },
                "required": ["typeName", "memberName"]
            })
        },
    },
    ToolDef {
        name: "getConstructors",
        description: "Конструкторы типа 1С (bsl-context).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "typeName": { "type": "string" }
                },
                "required": ["typeName"]
            })
        },
    },
    // ── P1 ───────────────────────────────────────────────────────────────────
    ToolDef {
        name: "call_graph",
        description: concat!(
            "Граф вызовов (рекурсивный call hierarchy). depth_up, depth_down, max_nodes ограничивают обход Java. ",
            page_note!(),
            " detail=full возвращает дерево целиком, а не плоскую страницу."
        ),
        input_schema: || {
            let mut schema = position_schema();
            if let Some(props) = schema
                .get_mut("properties")
                .and_then(|value| value.as_object_mut())
            {
                props.insert(
                    "depth_up".into(),
                    json!({"type":"integer","description":"Глубина callers (default 5)"}),
                );
                props.insert(
                    "depth_down".into(),
                    json!({"type":"integer","description":"Глубина callees (default 5)"}),
                );
                props.insert(
                    "max_nodes".into(),
                    json!({"type":"integer","description":"Лимит узлов (default 100)"}),
                );
            }
            insert_page_props(&mut schema);
            schema
        },
    },
    ToolDef {
        name: "symbol_impact",
        description: concat!(
            "Blast radius: incoming callers, группировка по типу модуля и страница references. ",
            page_note!()
        ),
        input_schema: || {
            let mut schema = position_schema();
            insert_page_props(&mut schema);
            schema
        },
    },
    ToolDef {
        name: "complexity",
        description: "Цикломатическая и когнитивная сложность методов модуля (CodeLens BSL LS).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": { "file_path": { "type": "string", "description": FILE_PATH_DESC } },
                "required": ["file_path"]
            })
        },
    },
    ToolDef {
        name: "quality_diagnostics",
        description: "Security / performance / SQL диагностики файла. categories — необязательный фильтр.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": FILE_PATH_DESC },
                    "categories": { "type": "string", "description": "security,performance,sql через запятую" }
                },
                "required": ["file_path"]
            })
        },
    },
    ToolDef {
        name: "module_health",
        description: "Сводка complexity + quality_diagnostics с приоритетом рефакторинга.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": { "file_path": { "type": "string", "description": FILE_PATH_DESC } },
                "required": ["file_path"]
            })
        },
    },
    ToolDef {
        name: "did_change_watched_files",
        description: "Уведомить LSP о внешних изменениях файлов (workspace/didChangeWatchedFiles).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "changes": {
                        "type": "array",
                        "description": "Массив {uri, type} где type: 1=Created 2=Changed 3=Deleted"
                    }
                },
                "required": ["changes"]
            })
        },
    },
    // ── P2 ───────────────────────────────────────────────────────────────────
    ToolDef {
        name: "signature_help",
        description: "Подсказки параметров в вызове (textDocument/signatureHelp). \
Interactive-таймаут ~10с; при таймауте следующие LSP-tools не блокируются.",
        input_schema: position_schema,
    },
    ToolDef {
        name: "inlay_hints",
        description: "Inlay hints в диапазоне строк файла.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": FILE_PATH_DESC },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["file_path", "start_line", "end_line"]
            })
        },
    },
    ToolDef {
        name: "selection_range",
        description: "Расширение выделения (textDocument/selectionRange).",
        input_schema: position_schema,
    },
    ToolDef {
        name: "semantic_tokens",
        description: "Семантические токены диапазона строк.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": FILE_PATH_DESC },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["file_path", "start_line", "end_line"]
            })
        },
    },
    ToolDef {
        name: "get_range_content",
        description: "Текст файла между start_line и end_line (inclusive, 0-based).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": FILE_PATH_DESC },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["file_path", "start_line", "end_line"]
            })
        },
    },
    ToolDef {
        name: "symbol_explore",
        description: "Поиск символов по проекту (обёртка workspace_symbols с limit).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "description": "default 20" }
                },
                "required": ["query"]
            })
        },
    },
    ToolDef {
        name: "project_analysis",
        description: "Обзор: analysis_type=workspace_symbols|file_analysis|workspace_analysis; query — имя/путь.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "analysis_type": { "type": "string" },
                    "query": { "type": "string" }
                },
                "required": ["analysis_type", "query"]
            })
        },
    },
];

fn is_platform_tool(name: &str) -> bool {
    PLATFORM_TOOLS.contains(&name)
}

fn unified_tool_names() -> Vec<&'static str> {
    UNIFIED_TOOLS.iter().map(|t| t.name).collect()
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_post(
    State(state): State<McpState>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    // Notifications (no id) — acknowledge without body
    if request.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.unwrap();
    let project_id = headers
        .get("x-project-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let response = match request.method.as_str() {
        "initialize" => handle_initialize(state.server_name, state.server_instructions, id),
        "ping" => JsonRpcResponse::success(id, json!({})),
        "tools/list" => handle_tools_list(state.tools, id),
        "tools/call" => {
            handle_tools_call(
                &state.manager,
                &state.bsl_context,
                state.tools,
                id,
                request.params,
                project_id,
            )
            .await
        }
        other => JsonRpcResponse::error(id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
    };

    Json(response).into_response()
}

/// GET /mcp — SSE stream for server-initiated notifications (not used yet).
async fn handle_get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

/// DELETE /mcp — session termination (stateless, always OK).
async fn handle_delete() -> StatusCode {
    StatusCode::OK
}

// ── MCP method implementations ───────────────────────────────────────────────

fn handle_initialize(server_name: &str, server_instructions: &str, id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": server_name,
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": server_instructions
        }),
    )
}

fn handle_tools_list(tools: &[ToolDef], id: Value) -> JsonRpcResponse {
    let list: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.input_schema)()
            })
        })
        .collect();

    JsonRpcResponse::success(id, json!({ "tools": list }))
}

async fn handle_tools_call(
    manager: &LspManager,
    bsl_context: &BslContextService,
    tools: &[ToolDef],
    id: Value,
    params: Option<Value>,
    project_id: Option<String>,
) -> JsonRpcResponse {
    let params = params.unwrap_or(Value::Null);
    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    if !tools.iter().any(|t| t.name == tool_name) {
        return tool_error(id, format!("Неизвестный инструмент: {tool_name}"));
    }

    if is_platform_tool(tool_name) {
        let result = call_platform_tool(manager, bsl_context, tool_name, &arguments).await;
        return match result {
            Ok(text) => JsonRpcResponse::success(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }]
                }),
            ),
            Err(err) => tool_error(id, err),
        };
    }

    let Some(project_id) = project_id else {
        return JsonRpcResponse::error(
            id,
            INVALID_REQUEST,
            "Заголовок x-project-id обязателен".to_string(),
        );
    };

    let result = match tool_name {
        "diagnostics" => call_diagnostics(manager, &project_id, &arguments).await,
        "status" => call_status(manager, &project_id).await,
        "hover" => call_hover(manager, &project_id, &arguments).await,
        "completion" => call_completion(manager, &project_id, &arguments).await,
        "code_actions" => call_code_actions(manager, &project_id, &arguments).await,
        "prepare_rename" => call_prepare_rename(manager, &project_id, &arguments).await,
        "rename" => call_rename(manager, &project_id, &arguments).await,
        "symbols" => call_symbols(manager, &project_id, &arguments).await,
        "definition" => call_definition(manager, &project_id, &arguments).await,
        "references" => call_references(manager, &project_id, &arguments).await,
        "incoming_calls" => call_incoming_calls(manager, &project_id, &arguments).await,
        "outgoing_calls" => call_outgoing_calls(manager, &project_id, &arguments).await,
        "workspace_symbols" => call_workspace_symbols(manager, &project_id, &arguments).await,
        "call_graph" => call_call_graph(manager, &project_id, &arguments).await,
        "symbol_impact" => call_symbol_impact(manager, &project_id, &arguments).await,
        "complexity" => call_complexity(manager, &project_id, &arguments).await,
        "quality_diagnostics" => call_quality_diagnostics(manager, &project_id, &arguments).await,
        "module_health" => call_module_health(manager, &project_id, &arguments).await,
        "did_change_watched_files" => {
            call_did_change_watched_files(manager, &project_id, &arguments).await
        }
        "signature_help" => call_signature_help(manager, &project_id, &arguments).await,
        "inlay_hints" => call_inlay_hints(manager, &project_id, &arguments).await,
        "selection_range" => call_selection_range(manager, &project_id, &arguments).await,
        "semantic_tokens" => call_semantic_tokens(manager, &project_id, &arguments).await,
        "get_range_content" => call_get_range_content(manager, &project_id, &arguments).await,
        "symbol_explore" => call_symbol_explore(manager, &project_id, &arguments).await,
        "project_analysis" => call_project_analysis(manager, &project_id, &arguments).await,
        _ => Err(format!("Неизвестный инструмент: {tool_name}")),
    };

    match result {
        Ok(value) => {
            let use_toon = manager.current_config().await.use_toon_format;
            let text = lsp_skill_core::toon::format_response(tool_name, &value, use_toon);
            JsonRpcResponse::success(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }]
                }),
            )
        }
        Err(err) => tool_error(id, err),
    }
}

async fn call_platform_tool(
    manager: &LspManager,
    bsl_context: &BslContextService,
    tool_name: &str,
    arguments: &Value,
) -> Result<String, String> {
    let cfg = manager.current_config().await;
    if !cfg.bsl_context_configured() {
        return Err(
            "bsl-context не настроен: задайте platform_path и bsl_context_jar_path в настройках"
                .to_string(),
        );
    }
    let client = bsl_context
        .ensure("java", &cfg.bsl_context_jar_path, &cfg.platform_path)
        .await
        .map_err(|e| e.to_string())?;
    let result = client
        .call_tool(tool_name, arguments.clone())
        .await
        .map_err(|e| e.to_string())?;
    tool_result_text(&result).map_err(|e| e.to_string())
}

fn tool_error(id: Value, message: String) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true
        }),
    )
}

// ── Tool call implementations ────────────────────────────────────────────────

fn extract_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Параметр {key} обязателен"))
}

fn extract_u32(args: &Value, key: &str) -> Result<u32, String> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| format!("Параметр {key} обязателен"))
}

async fn call_diagnostics(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    manager
        .diagnostics(project_id, file_path)
        .await
        .map_err(|e| e.to_string())
}

async fn call_status(manager: &LspManager, project_id: &str) -> Result<Value, String> {
    let snapshot = manager
        .get_project(project_id)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(snapshot).map_err(|e| e.to_string())
}

async fn call_hover(manager: &LspManager, project_id: &str, args: &Value) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .hover(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_completion(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .completion(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_code_actions(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .code_actions(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_prepare_rename(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .prepare_rename(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_rename(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    let new_name = extract_str(args, "new_name")?;
    let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);

    let edit = manager
        .rename(project_id, file_path, line, character, new_name)
        .await
        .map_err(|e| e.to_string())?;

    if apply {
        // Применение WorkspaceEdit на диск — отдельный шаг; пока возвращаем edit
        // и явную пометку, что apply ещё не реализован безопасным writer'ом.
        return Ok(json!({
            "applied": false,
            "warning": "apply=true пока не записывает файлы: используйте preview (apply=false) и примените правки вручную/через IDE. WorkspaceEdit ниже.",
            "edit": edit
        }));
    }

    Ok(json!({
        "applied": false,
        "edit": edit
    }))
}

async fn call_symbols(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    manager
        .symbols(project_id, file_path)
        .await
        .map_err(|e| e.to_string())
}

async fn call_definition(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .definition(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_references(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    let query = page_query_from_args(args)?;
    let value = manager
        .references(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())?;
    Ok(lsp_skill_core::page::present_list(&value, query))
}

async fn call_workspace_symbols(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let query = extract_str(args, "query")?;
    let page = page_query_from_args(args)?;
    let value = manager
        .workspace_symbols(project_id, query)
        .await
        .map_err(|e| e.to_string())?;
    Ok(lsp_skill_core::page::present_list(&value, page))
}

async fn call_incoming_calls(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .incoming_calls(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_outgoing_calls(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .outgoing_calls(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_call_graph(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    let depth_up = args.get("depth_up").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
    let depth_down = args.get("depth_down").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
    let max_nodes = args
        .get("max_nodes")
        .and_then(|v| v.as_u64())
        .unwrap_or(100) as usize;
    let page = page_query_from_args(args)?;
    let value = manager
        .call_graph(
            project_id, file_path, line, character, depth_up, depth_down, max_nodes,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(lsp_skill_core::page::present_call_graph(&value, page))
}

async fn call_symbol_impact(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    let page = page_query_from_args(args)?;
    let mut value = manager
        .symbol_impact(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(references) = value.get("references").cloned() {
        value["references"] = lsp_skill_core::page::present_list(&references, page);
    }
    Ok(value)
}

async fn call_complexity(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    manager
        .complexity(project_id, file_path)
        .await
        .map_err(|e| e.to_string())
}

async fn call_quality_diagnostics(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let categories: Vec<String> = args
        .get("categories")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    manager
        .quality_diagnostics(project_id, file_path, &categories)
        .await
        .map_err(|e| e.to_string())
}

async fn call_module_health(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    manager
        .module_health(project_id, file_path)
        .await
        .map_err(|e| e.to_string())
}

async fn call_did_change_watched_files(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let changes = args
        .get("changes")
        .and_then(|v| v.as_array())
        .cloned()
        .ok_or_else(|| "Параметр changes обязателен".to_string())?;
    manager
        .did_change_watched_files(project_id, changes)
        .await
        .map_err(|e| e.to_string())
}

async fn call_signature_help(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .signature_help(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_inlay_hints(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let start_line = extract_u32(args, "start_line")?;
    let end_line = extract_u32(args, "end_line")?;
    manager
        .inlay_hints(project_id, file_path, start_line, end_line)
        .await
        .map_err(|e| e.to_string())
}

async fn call_selection_range(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let line = extract_u32(args, "line")?;
    let character = extract_u32(args, "character")?;
    manager
        .selection_range(project_id, file_path, line, character)
        .await
        .map_err(|e| e.to_string())
}

async fn call_semantic_tokens(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let start_line = extract_u32(args, "start_line")?;
    let end_line = extract_u32(args, "end_line")?;
    manager
        .semantic_tokens(project_id, file_path, start_line, end_line)
        .await
        .map_err(|e| e.to_string())
}

async fn call_get_range_content(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let file_path = extract_str(args, "file_path")?;
    let start_line = extract_u32(args, "start_line")?;
    let end_line = extract_u32(args, "end_line")?;
    manager
        .get_range_content(project_id, file_path, start_line, end_line)
        .await
        .map_err(|e| e.to_string())
}

async fn call_symbol_explore(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let query = extract_str(args, "query")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    manager
        .symbol_explore(project_id, query, limit)
        .await
        .map_err(|e| e.to_string())
}

async fn call_project_analysis(
    manager: &LspManager,
    project_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let analysis_type = extract_str(args, "analysis_type")?;
    let query = extract_str(args, "query")?;
    manager
        .project_analysis(project_id, analysis_type, query)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_tools_cover_base_set_without_duplicates() {
        let names = unified_tool_names();
        let required = [
            "diagnostics",
            "status",
            "hover",
            "completion",
            "code_actions",
            "prepare_rename",
            "rename",
            "symbols",
            "definition",
            "references",
            "workspace_symbols",
            "incoming_calls",
            "outgoing_calls",
            "search",
            "info",
            "getMembers",
            "getMember",
            "getConstructors",
            "call_graph",
            "symbol_impact",
            "complexity",
            "quality_diagnostics",
            "module_health",
            "did_change_watched_files",
            "signature_help",
            "inlay_hints",
            "selection_range",
            "semantic_tokens",
            "get_range_content",
            "symbol_explore",
            "project_analysis",
        ];
        for name in required {
            assert!(names.contains(&name), "missing tool {name} in {names:?}");
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate tool names in UNIFIED_TOOLS"
        );
    }

    #[test]
    fn unified_tool_schemas_are_objects_with_required_fields() {
        for tool in UNIFIED_TOOLS {
            let schema = (tool.input_schema)();
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "tool {} schema.type",
                tool.name
            );
            match tool.name {
                "diagnostics" | "symbols" => {
                    let required = schema["required"].as_array().expect("required");
                    assert!(required.iter().any(|v| v.as_str() == Some("file_path")));
                }
                "definition" | "references" | "incoming_calls" | "outgoing_calls" | "hover"
                | "completion" | "code_actions" | "prepare_rename" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["file_path", "line", "character"] {
                        assert!(
                            required.iter().any(|v| v.as_str() == Some(key)),
                            "tool {} missing required {key}",
                            tool.name
                        );
                    }
                }
                "rename" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["file_path", "line", "character", "new_name"] {
                        assert!(
                            required.iter().any(|v| v.as_str() == Some(key)),
                            "tool rename missing required {key}"
                        );
                    }
                }
                "workspace_symbols" => {
                    let required = schema["required"].as_array().expect("required");
                    assert!(required.iter().any(|v| v.as_str() == Some("query")));
                }
                "status" => {}
                "search" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["query", "type", "limit"] {
                        assert!(required.iter().any(|v| v.as_str() == Some(key)));
                    }
                }
                "info" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["name", "type"] {
                        assert!(required.iter().any(|v| v.as_str() == Some(key)));
                    }
                }
                "getMembers" | "getConstructors" => {
                    let required = schema["required"].as_array().expect("required");
                    assert!(required.iter().any(|v| v.as_str() == Some("typeName")));
                }
                "getMember" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["typeName", "memberName"] {
                        assert!(required.iter().any(|v| v.as_str() == Some(key)));
                    }
                }
                "call_graph" | "symbol_impact" | "signature_help" | "selection_range" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["file_path", "line", "character"] {
                        assert!(required.iter().any(|v| v.as_str() == Some(key)));
                    }
                }
                "complexity" | "quality_diagnostics" | "module_health" => {
                    let required = schema["required"].as_array().expect("required");
                    assert!(required.iter().any(|v| v.as_str() == Some("file_path")));
                }
                "did_change_watched_files" => {
                    let required = schema["required"].as_array().expect("required");
                    assert!(required.iter().any(|v| v.as_str() == Some("changes")));
                }
                "inlay_hints" | "semantic_tokens" | "get_range_content" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["file_path", "start_line", "end_line"] {
                        assert!(required.iter().any(|v| v.as_str() == Some(key)));
                    }
                }
                "symbol_explore" => {
                    let required = schema["required"].as_array().expect("required");
                    assert!(required.iter().any(|v| v.as_str() == Some("query")));
                }
                "project_analysis" => {
                    let required = schema["required"].as_array().expect("required");
                    for key in ["analysis_type", "query"] {
                        assert!(required.iter().any(|v| v.as_str() == Some(key)));
                    }
                }
                other => panic!("unexpected tool in UNIFIED_TOOLS: {other}"),
            }
            if matches!(
                tool.name,
                "references" | "workspace_symbols" | "call_graph" | "symbol_impact"
            ) {
                for key in ["limit", "offset", "detail"] {
                    assert!(
                        schema["properties"].get(key).is_some(),
                        "tool {} missing page property {key}",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn tools_list_json_contains_all_unified_names() {
        let response = handle_tools_list(UNIFIED_TOOLS, json!(1));
        let tools = response
            .result
            .expect("result")
            .get("tools")
            .expect("tools")
            .as_array()
            .expect("array")
            .clone();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|v| v.as_str()).unwrap())
            .collect();
        assert_eq!(names, unified_tool_names());
    }
}
