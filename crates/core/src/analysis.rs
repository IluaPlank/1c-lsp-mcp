//! Pure helpers for P1 analysis tools (call graph, impact, quality, complexity).

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{json, Value};

/// Classify a BSL module path/URI into a 1C module type label.
pub fn classify_module_type(uri_or_path: &str) -> &'static str {
    let p = uri_or_path.replace('\\', "/").to_lowercase();
    let base = p.rsplit('/').next().unwrap_or("");

    if p.contains("/forms/") && (base == "module.bsl" || p.ends_with("form/module.bsl")) {
        return "FormModule";
    }
    match base {
        "managermodule.bsl" => return "ManagerModule",
        "objectmodule.bsl" => return "ObjectModule",
        "recordsetmodule.bsl" => return "RecordSetModule",
        "valuemanagermodule.bsl" => return "ValueManagerModule",
        "commandmodule.bsl" => return "CommandModule",
        "module.bsl" if p.contains("/commonmodules/") => return "CommonModule",
        _ => {}
    }
    if p.contains("/commonmodules/") {
        return "CommonModule";
    }
    if p.contains("/forms/") {
        return "FormModule";
    }
    "Module"
}

const BSL_ENTRY_POINTS: &[&str] = &[
    "ПриЗаписи",
    "ПриПроведении",
    "ПриОтменеПроведения",
    "ПередЗаписью",
    "ПередУдалением",
    "ПриУстановкеНовогоНомера",
    "ПриКопировании",
    "ОбработкаЗаполнения",
    "ОбработкаПроверкиЗаполнения",
    "ПриСозданииНаСервере",
    "ПриОткрытии",
    "ПриЗакрытии",
    "ПередЗаписьюНаСервере",
    "ПриЗаписиНаСервере",
    "ПослеЗаписиНаСервере",
    "ПриЧтенииНаСервере",
    "ОбработкаОповещения",
    "ОбработкаНавигационнойСсылки",
    "ОбработкаКоманды",
    "ПриВыполнении",
    "ПриНачалеРаботыСистемы",
    "ПриЗавершенииРаботыСистемы",
    "ПередНачаломРаботыСистемы",
    "ПередЗавершениемРаботыСистемы",
    "ОбработчикРегламентногоЗадания",
    "ОбработкаВызоваHTTPСервиса",
    "ОбработкаВызоваWebСервиса",
    "OnWrite",
    "Posting",
    "OnOpen",
    "OnCreateAtServer",
    "BeforeWrite",
    "OnClose",
];

pub fn is_bsl_entry_point(name: &str) -> bool {
    BSL_ENTRY_POINTS.iter().any(|e| *e == name)
}

#[derive(Debug, Clone)]
pub struct CallGraphLimits {
    pub depth_up: u32,
    pub depth_down: u32,
    pub max_nodes: usize,
}

impl Default for CallGraphLimits {
    fn default() -> Self {
        Self {
            depth_up: 5,
            depth_down: 5,
            max_nodes: 100,
        }
    }
}

pub fn item_key(item: &Value) -> String {
    let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let line = item
        .pointer("/selectionRange/start/line")
        .or_else(|| item.pointer("/range/start/line"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let character = item
        .pointer("/selectionRange/start/character")
        .or_else(|| item.pointer("/range/start/character"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    format!("{uri}|{name}|{line}|{character}")
}

pub fn call_item_from_edge(edge: &Value, direction: &str) -> Option<Value> {
    let key = if direction == "up" { "from" } else { "to" };
    edge.get(key).cloned()
}

/// Iterative BFS call-graph builder.
/// `expand(item, "up"|"down")` returns call-hierarchy edges.
pub async fn build_call_graph_json<F, Fut>(
    root_item: Value,
    limits: CallGraphLimits,
    mut expand: F,
) -> Value
where
    F: FnMut(Value, &'static str) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Value>, String>>,
{
    let mut node_count = 0usize;
    let mut cycles = 0usize;
    let mut entry_points: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut truncate_reason = String::new();

    let root_name = root_item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("(root)")
        .to_string();
    if is_bsl_entry_point(&root_name) {
        entry_points.push(root_name.clone());
    }

    let incoming = build_tree_bfs(
        &root_item,
        "up",
        limits.depth_up,
        limits.max_nodes,
        &mut node_count,
        &mut cycles,
        &mut entry_points,
        &mut truncated,
        &mut truncate_reason,
        &mut expand,
    )
    .await;

    let outgoing = build_tree_bfs(
        &root_item,
        "down",
        limits.depth_down,
        limits.max_nodes,
        &mut node_count,
        &mut cycles,
        &mut entry_points,
        &mut truncated,
        &mut truncate_reason,
        &mut expand,
    )
    .await;

    entry_points.sort();
    entry_points.dedup();

    json!({
        "root": {
            "name": root_name,
            "uri": root_item.get("uri").cloned().unwrap_or(Value::Null),
            "key": item_key(&root_item),
            "is_entry_point": is_bsl_entry_point(root_item.get("name").and_then(|v| v.as_str()).unwrap_or("")),
        },
        "incoming_tree": incoming,
        "outgoing_tree": outgoing,
        "total_nodes": node_count,
        "cycles_found": cycles,
        "entry_points_found": entry_points,
        "truncated": truncated,
        "truncate_reason": if truncate_reason.is_empty() { Value::Null } else { json!(truncate_reason) },
        "limits": {
            "depth_up": limits.depth_up,
            "depth_down": limits.depth_down,
            "max_nodes": limits.max_nodes
        }
    })
}

async fn build_tree_bfs<F, Fut>(
    root: &Value,
    direction: &'static str,
    max_depth: u32,
    max_nodes: usize,
    node_count: &mut usize,
    cycles: &mut usize,
    entry_points: &mut Vec<String>,
    truncated: &mut bool,
    truncate_reason: &mut String,
    expand: &mut F,
) -> Value
where
    F: FnMut(Value, &'static str) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Value>, String>>,
{
    // nodes[key] = json node being built; children filled after expand
    let root_key = item_key(root);
    let mut nodes: HashMap<String, Value> = HashMap::new();
    let mut children_map: HashMap<String, Vec<Value>> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(root_key.clone());

    let mut queue: VecDeque<(Value, u32, String)> = VecDeque::new();
    queue.push_back((root.clone(), 0, root_key.clone()));

    while let Some((item, depth, key)) = queue.pop_front() {
        if *node_count >= max_nodes {
            *truncated = true;
            *truncate_reason = format!("max_nodes={max_nodes}");
            break;
        }
        *node_count += 1;

        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        if is_bsl_entry_point(&name) {
            entry_points.push(name.clone());
        }

        nodes.insert(
            key.clone(),
            json!({
                "name": name,
                "uri": item.get("uri").cloned().unwrap_or(Value::Null),
                "key": key,
                "depth": depth,
                "direction": direction,
                "module_type": classify_module_type(uri),
                "is_entry_point": is_bsl_entry_point(item.get("name").and_then(|v| v.as_str()).unwrap_or("")),
            }),
        );

        if depth >= max_depth {
            children_map.entry(key).or_default();
            continue;
        }

        let edges = match expand(item.clone(), direction).await {
            Ok(v) => v,
            Err(err) => {
                nodes.get_mut(&key).map(|n| {
                    if let Some(obj) = n.as_object_mut() {
                        obj.insert("error".into(), json!(err));
                    }
                });
                children_map.entry(key).or_default();
                continue;
            }
        };

        let mut child_refs = Vec::new();
        for edge in edges {
            let Some(child_item) = call_item_from_edge(&edge, direction) else {
                continue;
            };
            let child_key = item_key(&child_item);
            if visited.contains(&child_key) {
                *cycles += 1;
                child_refs.push(json!({
                    "name": child_item.get("name").cloned().unwrap_or(json!("")),
                    "uri": child_item.get("uri").cloned().unwrap_or(Value::Null),
                    "key": child_key,
                    "is_cycle": true,
                    "module_type": classify_module_type(
                        child_item.get("uri").and_then(|v| v.as_str()).unwrap_or("")
                    ),
                }));
                continue;
            }
            visited.insert(child_key.clone());
            child_refs.push(json!({ "ref": child_key }));
            queue.push_back((child_item, depth + 1, child_key));
        }
        children_map.insert(key, child_refs);
    }

    fn materialize(
        key: &str,
        nodes: &HashMap<String, Value>,
        children_map: &HashMap<String, Vec<Value>>,
    ) -> Value {
        let mut node = nodes.get(key).cloned().unwrap_or(json!({ "key": key }));
        let mut kids = Vec::new();
        if let Some(refs) = children_map.get(key) {
            for r in refs {
                if r.get("is_cycle").and_then(|v| v.as_bool()) == Some(true) {
                    kids.push(r.clone());
                } else if let Some(child_key) = r.get("ref").and_then(|v| v.as_str()) {
                    kids.push(materialize(child_key, nodes, children_map));
                }
            }
        }
        if let Some(obj) = node.as_object_mut() {
            obj.insert("children".into(), Value::Array(kids));
        }
        node
    }

    materialize(&root_key, &nodes, &children_map)
}

pub fn build_symbol_impact_json(
    symbol_name: &str,
    file_path: &str,
    line: u32,
    character: u32,
    callers: &[Value],
    refs: &Value,
) -> Value {
    let mut by_type: HashMap<String, Vec<Value>> = HashMap::new();
    for edge in callers {
        let from = edge.get("from").cloned().unwrap_or(Value::Null);
        let uri = from.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let module_type = classify_module_type(uri).to_string();
        by_type.entry(module_type).or_default().push(json!({
            "name": from.get("name").cloned().unwrap_or(json!("")),
            "uri": from.get("uri").cloned().unwrap_or(Value::Null),
            "fromRanges": edge.get("fromRanges").cloned().unwrap_or(json!([]))
        }));
    }

    let ref_count = refs.as_array().map(|a| a.len()).unwrap_or(0);
    let mut grouped = serde_json::Map::new();
    for (k, mut v) in by_type {
        v.sort_by(|a, b| {
            a.get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .cmp(b.get("name").and_then(|x| x.as_str()).unwrap_or(""))
        });
        grouped.insert(k, Value::Array(v));
    }

    json!({
        "symbol": symbol_name,
        "position": { "file_path": file_path, "line": line, "character": character },
        "incoming_callers_by_module_type": grouped,
        "incoming_callers_total": callers.len(),
        "references_total": ref_count,
        "references": refs,
        "note": "Call hierarchy = direct CALL edges only. Event subscriptions / string refs in queries may be missing — complement with grep."
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplexityRow {
    pub method: String,
    pub line: u32,
    pub cyclomatic: Option<u32>,
    pub cognitive: Option<u32>,
}

fn trailing_int(title: &str) -> Option<u32> {
    let digits: String = title
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

pub fn parse_complexity_lenses(lenses: &[Value]) -> Vec<ComplexityRow> {
    let mut map: HashMap<(u32, String), ComplexityRow> = HashMap::new();

    for lens in lenses {
        let line = lens
            .pointer("/range/start/line")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let title = lens
            .pointer("/command/title")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let method = lens
            .pointer("/command/arguments/0")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let value = trailing_int(title.trim());

        let key = (line, method.clone());
        let row = map.entry(key).or_insert(ComplexityRow {
            method,
            line,
            cyclomatic: None,
            cognitive: None,
        });
        let lower = title.to_lowercase();
        if lower.contains("цикл") || lower.contains("cyclomatic") {
            row.cyclomatic = value;
        } else if lower.contains("когнитив") || lower.contains("cognitive") {
            row.cognitive = value;
        } else if row.cyclomatic.is_none() {
            row.cyclomatic = value;
        }
    }

    let mut rows: Vec<_> = map.into_values().collect();
    rows.sort_by_key(|r| r.line);
    rows
}

pub fn complexity_rows_to_json(rows: &[ComplexityRow]) -> Value {
    let flagged: Vec<_> = rows
        .iter()
        .filter(|r| r.cyclomatic.unwrap_or(0) > 20 || r.cognitive.unwrap_or(0) > 15)
        .map(|r| {
            json!({
                "method": r.method,
                "line": r.line,
                "cyclomatic": r.cyclomatic,
                "cognitive": r.cognitive
            })
        })
        .collect();
    let all: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "method": r.method,
                "line": r.line,
                "cyclomatic": r.cyclomatic,
                "cognitive": r.cognitive,
                "over_threshold": r.cyclomatic.unwrap_or(0) > 20 || r.cognitive.unwrap_or(0) > 15
            })
        })
        .collect();
    json!({
        "methods": all,
        "flagged": flagged,
        "thresholds": { "cyclomatic": 20, "cognitive": 15 }
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
struct BslDiagnosticMeta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    tags: Vec<String>,
}

fn diagnostic_table() -> &'static HashMap<String, BslDiagnosticMeta> {
    use std::sync::OnceLock;
    static TABLE: OnceLock<HashMap<String, BslDiagnosticMeta>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let raw = include_str!("../data/bsl_diagnostic_meta.json");
        serde_json::from_str(raw).unwrap_or_default()
    })
}

pub fn quality_categories(code: &str) -> Vec<&'static str> {
    let Some(meta) = diagnostic_table().get(code) else {
        return Vec::new();
    };
    let mut cats = Vec::new();
    if meta.kind == "SECURITY_HOTSPOT" || meta.kind == "VULNERABILITY" {
        cats.push("security");
    }
    for t in &meta.tags {
        match t.as_str() {
            "PERFORMANCE" => cats.push("performance"),
            "SQL" => cats.push("sql"),
            _ => {}
        }
    }
    cats
}

pub fn filter_quality_diagnostics(diagnostics: &[Value], wanted: &[&str]) -> Value {
    let mut out = Vec::new();
    for d in diagnostics {
        let code = d
            .get("code")
            .and_then(|c| {
                c.as_str().map(|s| s.to_string()).or_else(|| {
                    c.get("value")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
            })
            .unwrap_or_default();
        let cats = quality_categories(&code);
        if cats.is_empty() {
            continue;
        }
        if !wanted.is_empty() && !cats.iter().any(|c| wanted.contains(c)) {
            continue;
        }
        out.push(json!({
            "code": code,
            "categories": cats,
            "message": d.get("message").cloned().unwrap_or(Value::Null),
            "severity": d.get("severity").cloned().unwrap_or(Value::Null),
            "range": d.get("range").cloned().unwrap_or(Value::Null),
        }));
    }
    json!({ "diagnostics": out, "count": out.len() })
}

pub fn merge_module_health(complexity: &Value, quality: &Value) -> Value {
    json!({
        "complexity": complexity,
        "quality": quality,
        "refactor_priority": complexity
            .get("flagged")
            .cloned()
            .unwrap_or(json!([])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_and_manager_modules() {
        assert_eq!(
            classify_module_type("file:///x/CommonModules/ОбщийМодульА/Ext/Module.bsl"),
            "CommonModule"
        );
        assert_eq!(
            classify_module_type("file:///x/Documents/Doc/Ext/ManagerModule.bsl"),
            "ManagerModule"
        );
        assert_eq!(
            classify_module_type("file:///x/Catalogs/X/Forms/Form/Ext/Form/Module.bsl"),
            "FormModule"
        );
    }

    #[test]
    fn parses_complexity_titles() {
        let lenses = vec![
            json!({
                "range": {"start": {"line": 3, "character": 0}},
                "command": {"title": "Цикломатическая сложность: 8", "arguments": ["ОбработатьДанные"]}
            }),
            json!({
                "range": {"start": {"line": 3, "character": 0}},
                "command": {"title": "Когнитивная сложность: 12", "arguments": ["ОбработатьДанные"]}
            }),
        ];
        let rows = parse_complexity_lenses(&lenses);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cyclomatic, Some(8));
        assert_eq!(rows[0].cognitive, Some(12));
    }

    #[test]
    fn filters_quality_by_category() {
        let table = diagnostic_table();
        assert!(!table.is_empty());
        let (code, _) = table
            .iter()
            .find(|(_, m)| {
                m.kind == "SECURITY_HOTSPOT"
                    || m.tags.iter().any(|t| t == "SQL" || t == "PERFORMANCE")
            })
            .expect("some quality rule");
        let cats = quality_categories(code);
        assert!(!cats.is_empty());
        let diags = vec![json!({
            "code": code,
            "message": "test",
            "severity": 2,
            "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
        })];
        let filtered = filter_quality_diagnostics(&diags, &cats);
        assert_eq!(filtered["count"], 1);
    }

    #[test]
    fn symbol_impact_groups_callers() {
        let callers = vec![json!({
            "from": {
                "name": "ЗапуститьЦепочку",
                "uri": "file:///CommonModules/A/Ext/Module.bsl"
            },
            "fromRanges": []
        })];
        let impact = build_symbol_impact_json(
            "ОбработатьДанные",
            "1c-src/x.bsl",
            2,
            10,
            &callers,
            &json!([]),
        );
        assert_eq!(impact["incoming_callers_total"], 1);
        assert!(impact["incoming_callers_by_module_type"]
            .get("CommonModule")
            .is_some());
    }

    #[tokio::test]
    async fn call_graph_bfs_respects_cycle_and_depth() {
        let root = json!({
            "name": "A",
            "uri": "file:///a.bsl",
            "selectionRange": {"start": {"line": 1, "character": 0}}
        });
        let b = json!({
            "name": "B",
            "uri": "file:///b.bsl",
            "selectionRange": {"start": {"line": 2, "character": 0}}
        });
        let graph = build_call_graph_json(
            root.clone(),
            CallGraphLimits {
                depth_up: 0,
                depth_down: 2,
                max_nodes: 50,
            },
            {
                let root = root.clone();
                let b = b.clone();
                move |item, direction| {
                    let root = root.clone();
                    let b = b.clone();
                    async move {
                        if direction == "down" && item_key(&item) == item_key(&root) {
                            Ok(vec![json!({"to": b})])
                        } else if direction == "down" && item_key(&item) == item_key(&b) {
                            Ok(vec![json!({"to": root})]) // cycle back
                        } else {
                            Ok(vec![])
                        }
                    }
                }
            },
        )
        .await;
        assert!(graph["cycles_found"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(graph["outgoing_tree"]["name"], "A");
    }
}
