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
    let mut model: Map<String, Value> = value
        .as_object()
        .cloned()
        .ok_or("upstream data item is not an object")?;
    let id = model
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("upstream data item has no id")?
        .to_string();
    model.insert("slug".into(), json!(id));
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

fn to_api_model(value: &Value) -> Result<Value, &'static str> {
    let slug = value
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| !slug.is_empty())
        .ok_or("upstream model has no slug")?;
    Ok(json!({
        "id": slug,
        "object": "model",
        "created": value.get("created").and_then(Value::as_u64).unwrap_or(0),
        "owned_by": value.get("owned_by").and_then(Value::as_str).unwrap_or("unknown"),
    }))
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
}
