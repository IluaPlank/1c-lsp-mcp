# 1c-mcp — краткие правила для агента

Один MCP-сервер `1c`: диагностика, навигация, анализ влияния, quality/complexity, справка платформы.

**Skill для агента:** `skills/1c-mcp/SKILL.md` (при Connect копируется в `<проект>/.cursor/skills/1c-mcp/`).

## Обязательно

- Code tools: заголовок `x-project-id`, `file_path` **relative** к `project_root_path`, координаты **0-based**.
- Platform tools (`search`, `info`, `getMembers`, `getMember`, `getConstructors`) — без `x-project-id`; нужны `platform_path` + `bsl_context_jar_path`.
- Перед тяжёлой навигацией: `status` (`ready` / `warming_up`).
- Для целых конфигураций: `call_graph`, `symbol_impact`, `references` вместо grep.

## Tools

| Группа | Tools |
|---|---|
| База | `diagnostics`, `status`, `symbols`, `definition`, `references`, `workspace_symbols`, `incoming_calls`, `outgoing_calls` |
| Точка кода | `hover`, `completion`, `signature_help`, `code_actions`, `prepare_rename`, `rename` |
| Влияние (P1) | `call_graph`, `symbol_impact`, `complexity`, `quality_diagnostics`, `module_health`, `did_change_watched_files` |
| Discovery (P2) | `inlay_hints`, `selection_range`, `semantic_tokens`, `get_range_content`, `symbol_explore`, `project_analysis` |
| Платформа | `search`, `info`, `getMembers`, `getMember`, `getConstructors` |

## Конфиг / установка

- Portable: `.\scripts\pack-portable.ps1 -Zip`, запуск из `portable/`
- `install.ps1`
- Skill: `skills/1c-mcp/SKILL.md`
