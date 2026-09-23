use std::sync::OnceLock;

use axum::http::{header, HeaderMap};
use serde_json::{json, Map, Value};

pub fn is_codex_client(headers: &HeaderMap, query: Option<&str>) -> bool {
    query.is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, version)| key == "client_version" && !version.is_empty())
    }) || headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("codex"))
}

pub fn for_client(value: Value, codex: bool) -> Result<Value, &'static str> {
    if codex {
        if value.get("models").and_then(Value::as_array).is_some() {
            let mut value = value;
            value.as_object_mut().unwrap().remove("data");
            return Ok(value);
        }
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or("upstream models response has no data or models array")?;
        let models = data
            .iter()
            .map(to_codex_model)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"models": models}))
    } else {
        if value.get("data").and_then(Value::as_array).is_some() {
            let mut value = value;
            value.as_object_mut().unwrap().remove("models");
            return Ok(value);
        }
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or("upstream models response has no data or models array")?;
        let data = models
            .iter()
            .map(to_api_model)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"object": "list", "data": data}))
    }
}

fn to_codex_model(value: &Value) -> Result<Value, &'static str> {
    let source = value
        .as_object()
        .ok_or("upstream data item is not an object")?;
    let id = source
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("upstream data item has no id")?;
    let mut model = source.clone();
    if let Some(defaults) = deepseek_defaults(id) {
        merge_missing(&mut model, defaults);
        if let Some(instructions) = source.get("base_instructions") {
            if source
                .get("model_messages")
                .and_then(|messages| messages.get("instructions_template"))
                .is_none()
            {
                model["model_messages"]["instructions_template"] = instructions.clone();
            }
        } else if let Some(instructions) = source
            .get("model_messages")
            .and_then(|messages| messages.get("instructions_template"))
        {
            model.insert("base_instructions".into(), instructions.clone());
        }
    }
    model.insert("slug".into(), json!(id));
    if id.starts_with("deepseek-") {
        enrich_deepseek_model(&mut model, source)?;
    }
    model.entry("display_name").or_insert_with(|| json!(id));
    model
        .entry("supported_reasoning_levels")
        .or_insert_with(|| json!([]));
    model
        .entry("shell_type")
        .or_insert_with(|| json!("unified_exec"));
    model.entry("visibility").or_insert_with(|| json!("list"));
    model
        .entry("supported_in_api")
        .or_insert_with(|| json!(true));
    model.entry("priority").or_insert_with(|| json!(0));
    model.entry("availability_nux").or_insert(Value::Null);
    model.entry("upgrade").or_insert(Value::Null);
    model
        .entry("support_verbosity")
        .or_insert_with(|| json!(false));
    model
        .entry("truncation_policy")
        .or_insert_with(|| json!({"mode": "tokens", "limit": 10000}));
    model
        .entry("experimental_supported_tools")
        .or_insert_with(|| json!([]));
    model
        .entry("input_modalities")
        .or_insert_with(|| json!(["text"]));
    model
        .entry("base_instructions")
        .or_insert_with(|| json!(""));
    Ok(Value::Object(model))
}

fn deepseek_defaults(id: &str) -> Option<&'static Map<String, Value>> {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    let catalog = CATALOG.get_or_init(|| {
        let mut catalog: Value = serde_json::from_str(include_str!("deepseek_defaults.json"))
            .expect("bundled DeepSeek model defaults must be valid JSON");
        let instructions = catalog["instructions_template"]
            .as_str()
            .expect("bundled DeepSeek instructions must be a string")
            .to_owned();
        for model in catalog["models"]
            .as_array_mut()
            .expect("bundled DeepSeek models must be an array")
        {
            let model = model
                .as_object_mut()
                .expect("bundled DeepSeek model must be an object");
            model.insert("base_instructions".into(), json!(instructions));
            model["model_messages"]["instructions_template"] = json!(instructions);
        }
        catalog
    });
    catalog
        .get("models")?
        .as_array()?
        .iter()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some(id))?
        .as_object()
}

fn merge_missing(model: &mut Map<String, Value>, defaults: &Map<String, Value>) {
    for (key, fallback) in defaults {
        if let Some(current) = model.get_mut(key) {
            if let (Value::Object(current), Value::Object(fallback)) = (current, fallback) {
                merge_missing(current, fallback);
            }
        } else {
            model.insert(key.clone(), fallback.clone());
        }
    }
}

fn enrich_deepseek_model(
    model: &mut Map<String, Value>,
    source: &Map<String, Value>,
) -> Result<(), &'static str> {
    if !source.contains_key("display_name") {
        if let Some(name) = source.get("name").and_then(Value::as_str) {
            model.insert("display_name".into(), json!(name));
        }
    }
    if !source.contains_key("max_context_window") {
        if let Some(context_window) = source.get("context_window") {
            model.insert("max_context_window".into(), context_window.clone());
        }
    }
    model
        .entry("shell_type")
        .or_insert_with(|| json!("shell_command"));

    let Some(effort) = source.get("effort") else {
        return Ok(());
    };
    let effort = effort
        .as_object()
        .ok_or("upstream deepseek effort is not an object")?;
    let default_level = effort
        .get("default_level")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("upstream deepseek default_level is not a string")
        })
        .transpose()?;
    let supported_levels = effort
        .get("supported_levels")
        .map(|value| {
            value
                .as_array()
                .ok_or("upstream deepseek supported_levels is not an array")?
                .iter()
                .map(|value| {
                    let level = value
                        .as_str()
                        .ok_or("upstream deepseek supported level is not a string")?;
                    let description = match level {
                        "low" => "Fast responses with lighter reasoning",
                        "high" => "Extra high reasoning depth for complex problems",
                        "max" => "Maximum reasoning depth for the hardest problems",
                        _ => level,
                    };
                    Ok(json!({"effort": level, "description": description}))
                })
                .collect::<Result<Vec<Value>, &'static str>>()
        })
        .transpose()?;
    if let Some(level) = default_level {
        if !source.contains_key("default_reasoning_level") {
            model.insert("default_reasoning_level".into(), json!(level));
        }
    }
    if let Some(levels) = supported_levels {
        if !source.contains_key("supported_reasoning_levels") {
            model.insert("supported_reasoning_levels".into(), json!(levels));
        }
    }
    Ok(())
}

fn to_api_model(value: &Value) -> Result<Value, &'static str> {
    let slug = value
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| !slug.is_empty())
        .ok_or("upstream model has no slug")?;
    let mut model = json!({
        "id": slug,
        "object": "model",
        "created": value.get("created").and_then(Value::as_u64).unwrap_or(0),
        "owned_by": value.get("owned_by").and_then(Value::as_str).unwrap_or(if slug.starts_with("deepseek-") { "deepseek" } else { "unknown" }),
    });
    if slug.starts_with("deepseek-") {
        let fields = model.as_object_mut().ok_or("invalid model object")?;
        if value.get("created").is_none() {
            fields.remove("created");
        }
        if let Some(name) = value.get("name").or_else(|| value.get("display_name")) {
            fields.insert("name".into(), name.clone());
        }
        for field in [
            "context_window",
            "max_output_tokens",
            "input_modalities",
            "output_modalities",
            "api_capabilities",
        ] {
            if let Some(field_value) = value.get(field) {
                fields.insert(field.into(), field_value.clone());
            }
        }
        let mut effort = Map::new();
        if let Some(default_level) = value.get("default_reasoning_level") {
            effort.insert("default_level".into(), default_level.clone());
        }
        if let Some(levels) = value
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
        {
            effort.insert(
                "supported_levels".into(),
                Value::Array(
                    levels
                        .iter()
                        .filter_map(|level| level.get("effort").cloned())
                        .collect(),
                ),
            );
        }
        if !effort.is_empty() {
            fields.insert("effort".into(), Value::Object(effort));
        }
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_codex_query_or_user_agent() {
        let mut headers = HeaderMap::new();
        assert!(!is_codex_client(&headers, None));
        assert!(!is_codex_client(&headers, Some("not_client_version=1.0")));
        assert!(is_codex_client(
            &headers,
            Some("limit=1&client_version=1.0")
        ));
        headers.insert(header::USER_AGENT, "Codex_CLI_RS/1.0".parse().unwrap());
        assert!(is_codex_client(&headers, None));
    }

    #[test]
    fn converts_catalogs_and_preserves_native_metadata() {
        let data = json!({"object": "list", "data": [{"id": "gpt-6-sol", "created": 1790035200, "owned_by": "openai", "context_window": 272000}]});
        let codex = for_client(data.clone(), true).unwrap();
        assert_eq!(codex["models"][0]["slug"], "gpt-6-sol");
        assert_eq!(codex["models"][0]["context_window"], 272000);
        assert_eq!(codex["models"][0]["shell_type"], "unified_exec");
        assert_eq!(codex["models"][0]["truncation_policy"]["limit"], 10000);
        assert_eq!(for_client(data.clone(), false).unwrap(), data);

        let api = for_client(codex.clone(), false).unwrap();
        assert_eq!(api["data"][0]["id"], "gpt-6-sol");
        assert_eq!(api["data"][0]["created"], 1790035200);
        assert_eq!(api["data"][0]["owned_by"], "openai");
        assert_eq!(for_client(codex.clone(), true).unwrap(), codex);
        let both = json!({"models": [], "data": []});
        assert_eq!(
            for_client(both.clone(), true).unwrap(),
            json!({"models": []})
        );
        assert_eq!(for_client(both, false).unwrap(), json!({"data": []}));
        assert_eq!(
            for_client(json!({"data": []}), true).unwrap(),
            json!({"models": []})
        );
        assert_eq!(
            for_client(json!({"models": []}), false).unwrap(),
            json!({"object": "list", "data": []})
        );
    }

    #[test]
    fn rejects_invalid_upstream_catalog_instead_of_returning_empty_list() {
        assert!(for_client(json!({"error": "missing"}), true).is_err());
        assert!(for_client(json!({"data": [{}]}), true).is_err());
        assert!(for_client(json!({"models": [{}]}), false).is_err());
    }

    #[test]
    fn adapts_deepseek_model_metadata_in_both_directions() {
        let data = json!({"object": "list", "data": [
            {
                "id": "deepseek-flash", "object": "model", "owned_by": "deepseek",
                "name": "DeepSeek-V4.1-Flash", "context_window": 1048576,
                "max_output_tokens": 393216, "input_modalities": ["text", "image"],
                "output_modalities": ["text"],
                "effort": {"supported_levels": ["low", "high", "max"], "default_level": "high"},
                "api_capabilities": {"anthropic_messages": {"system_prompt_update": "in-history"}}
            },
            {
                "id": "deepseek-v4-pro", "object": "model", "owned_by": "deepseek",
                "name": "DeepSeek-V4-Pro", "context_window": 1048576,
                "max_output_tokens": 393216, "input_modalities": ["text"],
                "output_modalities": ["text"],
                "effort": {"supported_levels": ["low", "high", "max"], "default_level": "high"},
                "api_capabilities": {"anthropic_messages": {"system_prompt_update": "leading-only"}}
            }
        ]});
        let codex = for_client(data.clone(), true).unwrap();
        for (model, expected_name, expected_priority) in codex["models"]
            .as_array()
            .unwrap()
            .iter()
            .zip([("DeepSeek-V4.1-Flash", 1), ("DeepSeek-V4-Pro", 2)])
            .map(|(model, (name, priority))| (model, name, priority))
        {
            assert_eq!(model["display_name"], expected_name);
            assert_eq!(model["default_reasoning_level"], "high");
            assert_eq!(model["supported_reasoning_levels"][0]["effort"], "low");
            assert_eq!(model["supported_reasoning_levels"][2]["effort"], "max");
            assert_eq!(model["shell_type"], "shell_command");
            assert_eq!(model["max_context_window"], 1048576);
            assert_eq!(model["max_output_tokens"], 393216);
            assert_eq!(model["priority"], expected_priority);
            assert_eq!(model["support_verbosity"], true);
            assert_eq!(model["multi_agent_version"], "v2");
            assert!(!model["base_instructions"].as_str().unwrap().is_empty());
            assert_eq!(
                model["base_instructions"],
                model["model_messages"]["instructions_template"]
            );
        }
        assert_eq!(
            codex["models"][0]["input_modalities"],
            json!(["text", "image"])
        );
        assert_eq!(codex["models"][1]["input_modalities"], json!(["text"]));
        assert_eq!(for_client(codex, false).unwrap()["data"], data["data"]);
    }

    #[test]
    fn preserves_existing_deepseek_codex_fields_and_rejects_bad_effort() {
        let model = json!({
            "id": "deepseek-flash", "name": "From API",
            "display_name": "From Catalog", "shell_type": "disabled",
            "priority": 99, "model_messages": {"tools": null},
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "medium", "description": "Existing"}],
            "effort": {"default_level": "high", "supported_levels": ["low"]}
        });
        let converted = for_client(json!({"data": [model]}), true).unwrap();
        let result = &converted["models"][0];
        assert_eq!(result["display_name"], "From Catalog");
        assert_eq!(result["shell_type"], "disabled");
        assert_eq!(result["default_reasoning_level"], "medium");
        assert_eq!(result["supported_reasoning_levels"][0]["effort"], "medium");
        assert_eq!(result["priority"], 99);
        assert!(result["model_messages"].get("tools").is_some());
        assert_eq!(
            result["model_messages"]["instructions_template"],
            result["base_instructions"]
        );
        let custom = for_client(
            json!({"data": [{"id": "deepseek-flash", "base_instructions": "Custom instructions"}]}),
            true,
        )
        .unwrap();
        assert_eq!(
            custom["models"][0]["model_messages"]["instructions_template"],
            "Custom instructions"
        );
        assert!(for_client(
            json!({"data": [{"id": "deepseek-x", "effort": {"supported_levels": "low"}}]}),
            true
        )
        .is_err());
        assert_eq!(
            for_client(json!({"data": [{"id": "other", "name": "Other"}]}), true).unwrap()
                ["models"][0]["display_name"],
            "other"
        );
        let unknown = for_client(json!({"data": [{"id": "deepseek-new"}]}), true).unwrap();
        assert_eq!(unknown["models"][0]["priority"], 0);
        assert_eq!(unknown["models"][0]["base_instructions"], "");
    }
}
