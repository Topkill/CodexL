use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const NEXT_AI_GATEWAY_PROVIDER_NAME: &str = "next-ai-gateway";
pub const NEXT_AI_GATEWAY_API_KEY: &str = "codexl-next-ai-gateway";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfigFile {
    pub path: String,
    pub config: Value,
}

pub fn read_gateway_config() -> Result<GatewayConfigFile, String> {
    let path = gateway_config_path();
    ensure_gateway_config_file(&path)?;
    let content = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let config = serde_json::from_str::<Value>(&content).map_err(|err| err.to_string())?;
    if !config.is_object() {
        return Err("Gateway config must be a JSON object".to_string());
    }

    Ok(GatewayConfigFile {
        path: path.to_string_lossy().to_string(),
        config,
    })
}

pub fn codex_provider_base_url() -> Result<String, String> {
    let file = read_gateway_config()?;
    Ok(format!("{}/v1", gateway_origin_from_config(&file.config)))
}

pub fn gateway_health_url() -> Result<String, String> {
    let file = read_gateway_config()?;
    Ok(format!(
        "{}/health",
        gateway_origin_from_config(&file.config)
    ))
}

pub fn codex_provider_api_key() -> Result<String, String> {
    let file = read_gateway_config()?;
    Ok(codex_provider_api_key_from_config(&file.config))
}

pub fn write_codex_model_catalog(selected_model: &str) -> Result<String, String> {
    let file = read_gateway_config()?;
    let mut models = Vec::new();
    push_unique_model(&mut models, selected_model.trim());
    for model in gateway_model_options_from_config(&file.config) {
        push_unique_model(&mut models, &model);
    }
    if models.is_empty() {
        return Err("Gateway model catalog requires at least one model".to_string());
    }

    let catalog = json!({
        "models": models
            .iter()
            .enumerate()
            .map(|(index, model)| codex_model_catalog_item(model, index))
            .collect::<Vec<_>>(),
    });
    let path = codex_model_catalog_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let temp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(&catalog).map_err(|err| err.to_string())?;
    std::fs::write(&temp_path, format!("{}\n", content)).map_err(|err| err.to_string())?;
    std::fs::rename(&temp_path, &path).map_err(|err| err.to_string())?;

    Ok(path.to_string_lossy().to_string())
}

fn codex_model_catalog_item(model: &str, priority: usize) -> Value {
    json!({
        "slug": model,
        "display_name": model,
        "description": format!("NextAI Gateway model {}", model),
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            { "effort": "low", "description": "Low reasoning" },
            { "effort": "medium", "description": "Medium reasoning" },
            { "effort": "high", "description": "High reasoning" },
            { "effort": "xhigh", "description": "Extra high reasoning" }
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "availability_nux": Value::Null,
        "upgrade": Value::Null,
        "base_instructions": "You are Codex, a coding agent.",
        "supports_reasoning_summaries": true,
        "default_reasoning_summary": "none",
        "support_verbosity": true,
        "default_verbosity": "low",
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": "text_and_image",
        "truncation_policy": { "mode": "tokens", "limit": 10000 },
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": true,
        "context_window": 128000,
        "max_context_window": 128000,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": true
    })
}

pub fn gateway_model_options_from_config(config: &Value) -> Vec<String> {
    let mut models = Vec::new();
    let mut providers = Vec::new();
    if let Some(items) = config.get("Providers").and_then(Value::as_array) {
        providers.extend(items.iter());
    }
    if let Some(items) = config.get("providers").and_then(Value::as_array) {
        providers.extend(items.iter());
    }

    for provider in providers {
        let provider_name = provider
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if provider_name.is_empty() {
            continue;
        }
        for model in gateway_provider_models(provider) {
            let option = gateway_model_option(provider_name, &model);
            push_unique_model(&mut models, &option);
        }
    }

    models
}

fn gateway_provider_models(provider: &Value) -> Vec<String> {
    match provider.get("models") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(gateway_model_name)
            .collect::<Vec<_>>(),
        Some(Value::String(models)) => comma_list(models),
        _ => Vec::new(),
    }
}

fn gateway_model_name(item: &Value) -> Option<String> {
    if let Some(model) = item
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(model.to_string());
    }
    let object = item.as_object()?;
    for field in ["name", "id", "model"] {
        if let Some(model) = object
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(model.to_string());
        }
    }
    None
}

fn gateway_model_option(provider_name: &str, model_name: &str) -> String {
    let provider = provider_name.trim();
    let model = model_name.trim().trim_start_matches('/');
    if provider.is_empty() || model.is_empty() {
        return String::new();
    }
    if model.starts_with(&format!("{}/", provider)) {
        model.to_string()
    } else {
        format!("{}/{}", provider, model)
    }
}

fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn push_unique_model(models: &mut Vec<String>, model: &str) {
    let model = model.trim();
    if !model.is_empty() && !models.iter().any(|item| item == model) {
        models.push(model.to_string());
    }
}

fn gateway_origin_from_config(config: &Value) -> String {
    let host = config
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1");
    let connect_host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        value => value,
    };
    let host_part = if connect_host.contains(':') && !connect_host.starts_with('[') {
        format!("[{}]", connect_host)
    } else {
        connect_host.to_string()
    };
    let port = config
        .get("port")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0 && *value <= u16::MAX as u64)
        .unwrap_or(14589);

    format!("http://{}:{}", host_part, port)
}

fn codex_provider_api_key_from_config(config: &Value) -> String {
    let auth = config.get("auth");
    let auth_enabled = auth
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if auth_enabled {
        for keys in [
            auth.and_then(|value| value.get("principals")),
            auth.and_then(|value| value.get("keys")),
            config.get("principals"),
            config.get("keys"),
        ] {
            if let Some(key) = first_gateway_key(keys) {
                return key;
            }
        }
    }

    NEXT_AI_GATEWAY_API_KEY.to_string()
}

fn first_gateway_key(value: Option<&Value>) -> Option<String> {
    let items = value?.as_array()?;
    for item in items {
        if let Some(key) = item.as_str().map(str::trim).filter(|key| !key.is_empty()) {
            return Some(key.to_string());
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        for field in ["key", "apiKey", "api_key", "token"] {
            if let Some(key) = object
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
            {
                return Some(key.to_string());
            }
        }
    }
    None
}

pub fn write_gateway_config(config: Value) -> Result<GatewayConfigFile, String> {
    if !config.is_object() {
        return Err("Gateway config must be a JSON object".to_string());
    }

    let path = gateway_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let temp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    std::fs::write(&temp_path, format!("{}\n", content)).map_err(|err| err.to_string())?;
    std::fs::rename(&temp_path, &path).map_err(|err| err.to_string())?;

    Ok(GatewayConfigFile {
        path: path.to_string_lossy().to_string(),
        config,
    })
}

fn ensure_gateway_config_file(path: &PathBuf) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let content =
        serde_json::to_string_pretty(&default_gateway_config()).map_err(|err| err.to_string())?;
    std::fs::write(path, format!("{}\n", content)).map_err(|err| err.to_string())
}

fn gateway_config_path() -> PathBuf {
    env_path("CODEXL_NEXT_AI_GATEWAY_CONFIG_PATH")
        .or_else(|| env_path("GATEWAY_CONFIG_PATH"))
        .unwrap_or_else(|| gateway_home_dir().join("gateway.config.json"))
}

fn codex_model_catalog_path() -> PathBuf {
    gateway_home_dir().join("codex-model-catalog.json")
}

fn gateway_home_dir() -> PathBuf {
    env_path("CODEXL_NEXT_AI_GATEWAY_HOME")
        .unwrap_or_else(|| codexl_home_dir().join("next-ai-gateway"))
}

fn codexl_home_dir() -> PathBuf {
    super::super::codexl_home_dir()
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(expand_home_path)
}

fn expand_home_path(value: String) -> PathBuf {
    super::super::expand_home_path(value)
}

fn default_gateway_config() -> Value {
    json!({
        "host": "127.0.0.1",
        "port": 14589,
        "bodyLimitBytes": 52428800,
        "Providers": [],
        "auth": {
            "enabled": false
        },
        "billing": {
            "enabled": false
        },
        "billingQueue": {
            "enabled": false
        },
        "billingWebhook": {
            "enabled": false
        },
        "rawTrace": {
            "enabled": false,
            "mode": "disabled"
        },
        "agent": {
            "storage": {
                "type": "filesystem"
            },
            "mcpServers": []
        },
        "mcpGateway": {
            "enabled": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_gateway_model_catalog_advertises_image_and_search_capabilities() {
        let model = codex_model_catalog_item("Provider/model", 0);

        assert_eq!(model["input_modalities"], json!(["text", "image"]));
        assert_eq!(model["supports_image_detail_original"], json!(true));
        assert_eq!(model["supports_search_tool"], json!(true));
        assert_eq!(model["web_search_tool_type"], json!("text_and_image"));
    }
}
