---
name: 1c-mcp
description: >-
  Use MCP server `1c` for 1C:Enterprise 8.3 / BSL diagnostics, navigation,
  impact analysis, quality, and platform API help. Use when editing or reviewing
  `.bsl`/`.os` modules, common modules, documents, forms, when estimating change
  impact, fixing analyzer errors, looking up platform methods, or when the user
  mentions 1С, BSL, БСП, конфигурация, or MCP `1c`. Prefer these tools over grep
  for symbol-aware work. Do not use for Vanessa UI tests (separate MCP).
  Always reply to the user in Russian.
---

# 1c-mcp

Single MCP server **`1c`** (`http://127.0.0.1:9010/mcp`). Vanessa is a separate server.

## Language

Always reply to the user in **Russian**. Keep tool names, parameters, paths, and code identifiers unchanged.

## Before you start

1. Call `status`. You need `ready`. On `warming_up`, results may be incomplete; on `stopped` or `error`, stop and tell the user.
2. Code tools require the `x-project-id` header (usually already set in `.cursor/mcp.json`).
3. `file_path` is **relative** to `project_root_path`, not absolute. Keep Cyrillic in paths as-is.
4. `line` / `character` are **0-based**.

Platform tools (`search`, `info`, `getMembers`, `getMember`, `getConstructors`) are not bound to a project file. They need `platform_path` and `bsl_context_jar_path` configured.

## Which tool when

| Task | Tools |
|---|---|
| Check a module before and after edits | `diagnostics`, then `code_actions` when needed |
| Understand file structure | `symbols` |
| Where it is declared / who uses it | `definition`, `references` |
| Who calls it / what it calls | `incoming_calls`, `outgoing_calls` |
| Find a symbol by name | `workspace_symbols` |
| Change radius / call graph | `symbol_impact`, `call_graph` |
| Module complexity / quality | `complexity`, `quality_diagnostics`, `module_health` |
| Hint at the cursor | `hover`, `signature_help`. `completion` is the IDE completion list at a position; on large configurations it often times out and the agent usually does not need it |
| Rename | `prepare_rename`, then `rename` (`apply=true` only with explicit user consent) |
| Overview / exploration | `symbol_explore`, `project_analysis`, `get_range_content` |
| 1C platform API | `search`, then `info` / `getMembers` / `getMember` / `getConstructors` |
| Files on disk changed outside the editor | `did_change_watched_files` |

Use `references`, `call_graph`, and `symbol_impact` for configuration-wide symbol work. Plain grep is only a fallback when the language server cannot see the call.

## Typical flows

### Editing a module

1. `status` until `ready`
2. `diagnostics` on the file (baseline)
3. `symbols` / `definition` for unfamiliar calls
4. Edit the code
5. `diagnostics` again; for public procedures, `incoming_calls` or `references`

### Impact estimate

1. Find the symbol with `workspace_symbols` or `definition`
2. `symbol_impact` and/or `incoming_calls` plus `references`
3. Group findings by module, then change behavior

### Platform help

1. `search` by type or method name
2. `info` / `getMembers` / `getMember` / `getConstructors` for the specific member

## 1C code rules (short)

- Use Russian identifiers per 1C standards. Prefer **БСП** (Standard Subsystems Library) methods over hand-written equivalents.
- Create forms in code. Do not author form XML by hand.
- Do not read metadata or form XML in full. Fetch the needed piece with tools and small file fragments.
- Before committing BSL, run `diagnostics` on the changed modules.

## Response size

The preferred payload format is TOON (`use_toon_format = true` by default): the same fields, shorter than JSON.

`references`, `workspace_symbols`, and `call_graph` return a summary and the first page of `items` (up to 20) by default. `items` holds locations, symbols, or graph nodes. While `truncated=true`, request the next page with the same call and `offset=next_offset`. For a large list page without per-file grouping, use `detail=full` (up to 200). For the full graph tree, call `call_graph` with `detail=full`. `limit` is from 1 to 200.

`symbol_impact` truncates nested `references` the same way. Caller groups stay intact.

## Limits

- An empty `diagnostics` result is not proof the module is clean while the index is still warming up.
- The language server may miss dynamic calls (`Выполнить`, string names). Use grep as a secondary check in that case.
- `completion`, and sometimes `signature_help`, can time out (~10s) on large configurations. Use `hover`, `workspace_symbols`, or read the code.
