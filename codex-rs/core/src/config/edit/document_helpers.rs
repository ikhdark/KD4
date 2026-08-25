use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverableType;
use toml_edit::Array as TomlArray;
use toml_edit::InlineTable;
use toml_edit::Item as TomlItem;
use toml_edit::Table as TomlTable;
use toml_edit::Value as TomlValue;

pub(super) fn ensure_table_for_write(item: &mut TomlItem) -> Option<&mut TomlTable> {
    match item {
        TomlItem::Table(table) => Some(table),
        TomlItem::Value(value) => {
            if let Some(inline) = value.as_inline_table() {
                *item = TomlItem::Table(table_from_inline(inline));
                item.as_table_mut()
            } else {
                *item = TomlItem::Table(new_implicit_table());
                item.as_table_mut()
            }
        }
        TomlItem::None => {
            *item = TomlItem::Table(new_implicit_table());
            item.as_table_mut()
        }
        _ => None,
    }
}

pub(super) fn ensure_table_for_read(item: &mut TomlItem) -> Option<&mut TomlTable> {
    match item {
        TomlItem::Table(table) => Some(table),
        TomlItem::Value(value) => {
            let inline = value.as_inline_table()?;
            *item = TomlItem::Table(table_from_inline(inline));
            item.as_table_mut()
        }
        _ => None,
    }
}

pub(super) fn merge_inline_table(existing: &mut InlineTable, replacement: InlineTable) {
    existing.retain(|key, _| replacement.get(key).is_some());

    for (key, value) in replacement.iter() {
        if let Some(existing_value) = existing.get_mut(key) {
            let mut updated_value = value.clone();
            *updated_value.decor_mut() = existing_value.decor().clone();
            *existing_value = updated_value;
        } else {
            existing.insert(key.to_string(), value.clone());
        }
    }
}

fn table_from_inline(inline: &InlineTable) -> TomlTable {
    let mut table = new_implicit_table();
    for (key, value) in inline.iter() {
        let mut value = value.clone();
        let decor = value.decor_mut();
        decor.set_suffix("");
        table.insert(key, TomlItem::Value(value));
    }
    table
}

pub(super) fn new_implicit_table() -> TomlTable {
    let mut table = TomlTable::new();
    table.set_implicit(true);
    table
}

pub(super) fn parse_tool_suggest_disabled_tool(
    value: &TomlValue,
) -> Option<ToolSuggestDisabledTool> {
    let table = value.as_inline_table()?;
    let kind = match table.get("type").and_then(TomlValue::as_str) {
        Some("connector") => ToolSuggestDiscoverableType::Connector,
        Some("plugin") => ToolSuggestDiscoverableType::Plugin,
        _ => return None,
    };
    let id = table.get("id").and_then(TomlValue::as_str)?;
    Some(ToolSuggestDisabledTool {
        kind,
        id: id.to_string(),
    })
}

pub(super) fn parse_tool_suggest_disabled_tool_table(
    table: &TomlTable,
) -> Option<ToolSuggestDisabledTool> {
    let kind = match table.get("type").and_then(TomlItem::as_str) {
        Some("connector") => ToolSuggestDiscoverableType::Connector,
        Some("plugin") => ToolSuggestDiscoverableType::Plugin,
        _ => return None,
    };
    let id = table.get("id").and_then(TomlItem::as_str)?;
    Some(ToolSuggestDisabledTool {
        kind,
        id: id.to_string(),
    })
}

pub(super) fn tool_suggest_disabled_tools_value(
    disabled_tools: &[ToolSuggestDisabledTool],
) -> TomlItem {
    let mut array = TomlArray::new();
    for disabled_tool in disabled_tools {
        let mut table = InlineTable::new();
        table.insert(
            "type",
            match disabled_tool.kind {
                ToolSuggestDiscoverableType::Connector => "connector",
                ToolSuggestDiscoverableType::Plugin => "plugin",
            }
            .into(),
        );
        table.insert("id", disabled_tool.id.clone().into());
        array.push(table);
    }
    TomlItem::Value(array.into())
}
