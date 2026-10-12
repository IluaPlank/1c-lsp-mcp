//! Page a heavy LSP result before it is sent to an agent.
//!
//! The page always contains the items themselves. Callers walk the rest with
//! `next_offset`. Nothing here is a cache id.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const SUMMARY_LIMIT: usize = 20;
pub const FULL_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 200;
const MAX_FILE_GROUPS: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultDetail {
    Summary,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageQuery {
    pub offset: usize,
    pub limit: Option<usize>,
    pub detail: ResultDetail,
}

impl PageQuery {
    pub fn parse(
        offset: Option<u64>,
        limit: Option<u64>,
        detail: Option<&str>,
    ) -> Result<Self, String> {
        let detail = match detail.unwrap_or("summary") {
            "" | "summary" => ResultDetail::Summary,
            "full" => ResultDetail::Full,
            other => {
                return Err(format!(
                    "detail должен быть summary или full, получено: {other}"
                ));
            }
        };
        let limit = match limit {
            None => None,
            Some(0) => return Err("limit должен быть не меньше 1".to_string()),
            Some(value) => Some(usize::try_from(value).unwrap_or(usize::MAX).min(MAX_LIMIT)),
        };
        Ok(Self {
            offset: usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX),
            limit,
            detail,
        })
    }

    fn effective_limit(self) -> usize {
        self.limit.unwrap_or(match self.detail {
            ResultDetail::Summary => SUMMARY_LIMIT,
            ResultDetail::Full => FULL_LIMIT,
        })
    }
}

/// `value` is a JSON array, `null`, or one element.
pub fn present_list(value: &Value, query: PageQuery) -> Value {
    match value {
        Value::Array(items) => page_slice(items, query, true),
        Value::Null => page_slice(&[], query, true),
        other => page_slice(std::slice::from_ref(other), query, true),
    }
}

/// Summary is a flat page of nodes. `detail=full` is the original tree.
pub fn present_call_graph(graph: &Value, query: PageQuery) -> Value {
    if query.detail == ResultDetail::Full {
        let mut full = graph.clone();
        if let Some(obj) = full.as_object_mut() {
            obj.insert("detail".into(), json!("full"));
        }
        return full;
    }

    let mut flat = Vec::new();
    if let Some(tree) = graph.get("incoming_tree") {
        flatten_tree(tree, "up", &mut flat);
    }
    if let Some(tree) = graph.get("outgoing_tree") {
        flatten_tree(tree, "down", &mut flat);
    }
    let mut page = page_slice(&flat, query, false);
    if let (Some(page_obj), Some(graph_obj)) = (page.as_object_mut(), graph.as_object()) {
        for key in [
            "root",
            "total_nodes",
            "cycles_found",
            "entry_points_found",
            "truncated",
            "truncate_reason",
            "limits",
        ] {
            if let Some(value) = graph_obj.get(key) {
                if key == "truncated" {
                    page_obj.insert("graph_truncated".into(), value.clone());
                } else {
                    page_obj.insert(key.into(), value.clone());
                }
            }
        }
    }
    page
}

fn page_slice(items: &[Value], query: PageQuery, with_file_groups: bool) -> Value {
    let total = items.len();
    let limit = query.effective_limit();
    let (start, end) = if query.offset >= total {
        (query.offset, query.offset)
    } else {
        (query.offset, (query.offset + limit).min(total))
    };
    let page = if start >= total {
        Vec::new()
    } else {
        items[start..end].to_vec()
    };
    let truncated = end < total;
    let mut body = Map::new();
    body.insert(
        "detail".into(),
        json!(match query.detail {
            ResultDetail::Summary => "summary",
            ResultDetail::Full => "full",
        }),
    );
    body.insert("total".into(), json!(total));
    body.insert("offset".into(), json!(query.offset));
    body.insert("limit".into(), json!(limit));
    body.insert("returned".into(), json!(page.len()));
    body.insert("truncated".into(), json!(truncated));
    body.insert(
        "next_offset".into(),
        if truncated { json!(end) } else { Value::Null },
    );
    if with_file_groups && query.detail == ResultDetail::Summary {
        body.insert("by_file".into(), group_by_file(items));
    }
    body.insert("items".into(), Value::Array(page));
    Value::Object(body)
}

fn group_by_file(items: &[Value]) -> Value {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unknown = 0usize;
    for item in items {
        match item_uri(item) {
            Some(uri) => *counts.entry(uri).or_default() += 1,
            None => unknown += 1,
        }
    }
    let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    let truncated = rows.len() > MAX_FILE_GROUPS;
    let files: Vec<Value> = rows
        .into_iter()
        .take(MAX_FILE_GROUPS)
        .map(|(uri, count)| json!({ "uri": uri, "count": count }))
        .collect();
    json!({
        "files": files,
        "unknown": unknown,
        "truncated": truncated
    })
}

fn item_uri(item: &Value) -> Option<&str> {
    item.get("uri")
        .and_then(Value::as_str)
        .or_else(|| item.get("targetUri").and_then(Value::as_str))
        .or_else(|| item.pointer("/location/uri").and_then(Value::as_str))
        .or_else(|| item.get("location").and_then(Value::as_str))
}

fn flatten_tree(node: &Value, direction: &str, out: &mut Vec<Value>) {
    let Some(children) = node.get("children").and_then(Value::as_array) else {
        return;
    };
    for child in children {
        if child.get("is_cycle").and_then(Value::as_bool) == Some(true) {
            out.push(json!({
                "name": child.get("name").cloned().unwrap_or(Value::Null),
                "uri": child.get("uri").cloned().unwrap_or(Value::Null),
                "key": child.get("key").cloned().unwrap_or(Value::Null),
                "direction": direction,
                "module_type": child.get("module_type").cloned().unwrap_or(Value::Null),
                "is_cycle": true
            }));
            continue;
        }
        out.push(json!({
            "name": child.get("name").cloned().unwrap_or(Value::Null),
            "uri": child.get("uri").cloned().unwrap_or(Value::Null),
            "key": child.get("key").cloned().unwrap_or(Value::Null),
            "depth": child.get("depth").cloned().unwrap_or(Value::Null),
            "direction": child.get("direction").cloned().unwrap_or(json!(direction)),
            "module_type": child.get("module_type").cloned().unwrap_or(Value::Null),
            "is_entry_point": child.get("is_entry_point").cloned().unwrap_or(json!(false)),
            "is_cycle": false
        }));
        flatten_tree(child, direction, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locations(count: usize) -> Value {
        Value::Array(
            (0..count)
                .map(|index| {
                    json!({
                        "uri": format!("file:///mod{}.bsl", index % 3),
                        "range": {"start": {"line": index, "character": 0}}
                    })
                })
                .collect(),
        )
    }

    #[test]
    fn summary_page_keeps_items_and_groups_files() {
        let query = PageQuery::parse(None, None, None).unwrap();
        let page = present_list(&locations(25), query);
        assert_eq!(page["detail"], "summary");
        assert_eq!(page["total"], 25);
        assert_eq!(page["returned"], SUMMARY_LIMIT);
        assert_eq!(page["truncated"], true);
        assert_eq!(page["next_offset"], SUMMARY_LIMIT);
        assert_eq!(page["items"][0]["range"]["start"]["line"], 0);
        assert!(page["items"][0].get("uri").is_some());
        let files = page["by_file"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(
            files
                .iter()
                .map(|f| f["count"].as_u64().unwrap())
                .sum::<u64>(),
            25
        );
    }

    #[test]
    fn next_offset_returns_the_rest() {
        let query = PageQuery::parse(Some(20), None, None).unwrap();
        let page = present_list(&locations(25), query);
        assert_eq!(page["returned"], 5);
        assert_eq!(page["truncated"], false);
        assert!(page["next_offset"].is_null());
        assert_eq!(page["items"][0]["range"]["start"]["line"], 20);
    }

    #[test]
    fn full_detail_uses_larger_page_without_file_groups() {
        let query = PageQuery::parse(None, None, Some("full")).unwrap();
        let page = present_list(&locations(25), query);
        assert_eq!(page["detail"], "full");
        assert_eq!(page["returned"], 25);
        assert_eq!(page["truncated"], false);
        assert!(page.get("by_file").is_none());
    }

    #[test]
    fn limit_is_capped_and_zero_is_rejected() {
        let query = PageQuery::parse(None, Some(10_000), Some("full")).unwrap();
        assert_eq!(query.effective_limit(), MAX_LIMIT);
        let page = present_list(&locations(5), query);
        assert_eq!(page["limit"], MAX_LIMIT);
        assert!(PageQuery::parse(None, Some(0), None).is_err());
        assert!(PageQuery::parse(None, None, Some("cache_id")).is_err());
    }

    #[test]
    fn null_list_is_an_empty_page() {
        let query = PageQuery::parse(None, None, None).unwrap();
        let page = present_list(&Value::Null, query);
        assert_eq!(page["total"], 0);
        assert_eq!(page["items"], json!([]));
    }

    #[test]
    fn call_graph_summary_is_flat_and_full_keeps_the_tree() {
        let graph = json!({
            "root": {"name": "Корень"},
            "total_nodes": 3,
            "cycles_found": 0,
            "truncated": false,
            "incoming_tree": {
                "name": "Корень",
                "children": [{
                    "name": "Вызывающий",
                    "uri": "file:///a.bsl",
                    "depth": 1,
                    "direction": "up",
                    "module_type": "CommonModule",
                    "children": []
                }]
            },
            "outgoing_tree": {
                "name": "Корень",
                "children": [{
                    "name": "Вызываемый",
                    "uri": "file:///b.bsl",
                    "depth": 1,
                    "direction": "down",
                    "module_type": "CommonModule",
                    "children": []
                }]
            }
        });
        let summary = present_call_graph(&graph, PageQuery::parse(None, None, None).unwrap());
        assert_eq!(summary["detail"], "summary");
        assert_eq!(summary["root"]["name"], "Корень");
        assert_eq!(summary["total"], 2);
        assert_eq!(summary["items"][0]["name"], "Вызывающий");
        assert_eq!(summary["items"][1]["name"], "Вызываемый");
        assert!(summary.get("incoming_tree").is_none());

        let full = present_call_graph(&graph, PageQuery::parse(None, None, Some("full")).unwrap());
        assert_eq!(full["detail"], "full");
        assert_eq!(full["incoming_tree"]["children"][0]["name"], "Вызывающий");
    }
}
