use super::*;

use crate::cmd_mcp::proxy_upstream::{send_request_to_upstream, UpstreamClient};

pub(crate) fn owner_for_tool(
    session: &Arc<Mutex<McpSessionState>>,
    tool_name: &str,
) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.tool_owners.get(tool_name).cloned())
}

pub(crate) fn forward_name_for_tool(
    session: &Arc<Mutex<McpSessionState>>,
    tool_name: &str,
) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.tool_forward_names.get(tool_name).cloned())
}

pub(crate) fn owner_for_resource(
    session: &Arc<Mutex<McpSessionState>>,
    uri: &str,
) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.resource_owners.get(uri).cloned())
}

pub(crate) fn ensure_upstream_tools_loaded(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    if let Ok(guard) = session.lock() {
        if guard.upstream_tools_loaded {
            return Ok(guard.upstream_tools_cache.clone());
        }
    }
    refresh_upstream_tools(session, upstreams)
}

pub(crate) fn ensure_upstream_resources_loaded(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    if let Ok(guard) = session.lock() {
        if guard.upstream_resources_loaded {
            return Ok(guard.upstream_resources_cache.clone());
        }
    }
    refresh_upstream_resources(session, upstreams)
}

pub(crate) fn ensure_upstream_resource_templates_loaded(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    if let Ok(guard) = session.lock() {
        if guard.upstream_resource_templates_loaded {
            return Ok(guard.upstream_resource_templates_cache.clone());
        }
    }
    let mut templates = Vec::new();
    for upstream in upstreams.values_mut() {
        let response = send_request_to_upstream(
            upstream,
            &json!({
                "jsonrpc":"2.0",
                "id": format!("packet28-templates-list-{}", upstream.name),
                "method":"resources/templates/list"
            }),
        )?;
        if let Some(items) = response
            .get("result")
            .and_then(|value| value.get("resourceTemplates"))
            .and_then(Value::as_array)
        {
            templates.extend(items.iter().cloned());
        }
    }
    if let Ok(mut guard) = session.lock() {
        guard.upstream_resource_templates_cache = templates.clone();
        guard.upstream_resource_templates_loaded = true;
    }
    Ok(templates)
}

fn refresh_upstream_tools(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    let native_tool_names = native_tool_names();
    let mut discovered = BTreeMap::<String, Vec<(String, Value)>>::new();
    let mut tool_owners = BTreeMap::new();
    for upstream in upstreams.values_mut() {
        let response = send_request_to_upstream(
            upstream,
            &json!({
                "jsonrpc":"2.0",
                "id": format!("packet28-tools-refresh-{}", upstream.name),
                "method":"tools/list"
            }),
        )?;
        if let Some(items) = response
            .get("result")
            .and_then(|value| value.get("tools"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    discovered
                        .entry(name.to_string())
                        .or_default()
                        .push((upstream.name.clone(), item.clone()));
                }
            }
        }
    }

    let mut tool_forward_names = BTreeMap::new();
    let mut rendered_tools = Vec::new();
    for (name, entries) in discovered {
        let needs_namespace = entries.len() > 1 || native_tool_names.contains_key(&name);
        for (owner, item) in entries {
            let alias = if needs_namespace {
                namespaced_tool_name(&owner, &name)
            } else {
                name.clone()
            };
            tool_owners.insert(alias.clone(), owner.clone());
            tool_forward_names.insert(alias.clone(), name.clone());
            rendered_tools.push(annotated_tool_item(item, &alias, &owner, needs_namespace));
        }
    }
    rendered_tools.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });

    if let Ok(mut guard) = session.lock() {
        guard.tool_owners = tool_owners;
        guard.tool_forward_names = tool_forward_names;
        guard.upstream_tools_cache = rendered_tools.clone();
        guard.upstream_tools_loaded = true;
    }
    Ok(rendered_tools)
}

fn refresh_upstream_resources(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    let mut resource_owners = BTreeMap::new();
    let mut rendered_resources = Vec::new();
    for upstream in upstreams.values_mut() {
        let response = send_request_to_upstream(
            upstream,
            &json!({
                "jsonrpc":"2.0",
                "id": format!("packet28-resources-refresh-{}", upstream.name),
                "method":"resources/list"
            }),
        )?;
        if let Some(items) = response
            .get("result")
            .and_then(|value| value.get("resources"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                    resource_owners.insert(uri.to_string(), upstream.name.clone());
                    rendered_resources.push(item.clone());
                }
            }
        }
    }
    rendered_resources.sort_by(|left, right| {
        left.get("uri")
            .and_then(Value::as_str)
            .cmp(&right.get("uri").and_then(Value::as_str))
    });
    if let Ok(mut guard) = session.lock() {
        guard.resource_owners = resource_owners;
        guard.upstream_resources_cache = rendered_resources.clone();
        guard.upstream_resources_loaded = true;
    }
    Ok(rendered_resources)
}

fn namespaced_tool_name(owner: &str, name: &str) -> String {
    let prefix = format!("{owner}.");
    if name.starts_with(&prefix) {
        name.to_string()
    } else {
        format!("{owner}.{name}")
    }
}

fn annotated_tool_item(mut item: Value, alias: &str, owner: &str, namespaced: bool) -> Value {
    if let Some(obj) = item.as_object_mut() {
        obj.insert("name".to_string(), Value::String(alias.to_string()));
        if namespaced {
            let description = obj
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("Upstream MCP tool");
            obj.insert(
                "description".to_string(),
                Value::String(format!("{description} [via {owner}]")),
            );
        }
    }
    item
}

fn native_tool_names() -> BTreeMap<String, ()> {
    [
        "packet28.search",
        "packet28.fetch_tool_result",
        "packet28.fetch_context",
        "packet28.prepare_handoff",
        "packet28.read_regions",
        "packet28.write_state",
        "packet28.task_status",
        "packet28.capabilities",
    ]
    .into_iter()
    .map(|name| (name.to_string(), ()))
    .collect()
}
