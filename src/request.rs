use std::collections::HashMap;
use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use colored::*;

use crate::env::EnvManager;

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestFile {
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    pub query: Option<HashMap<String, String>>,
    pub body: Option<serde_yaml::Value>,
    pub exports: Option<HashMap<String, String>>,
}

pub struct RequestRunner {
    client: Client,
    env_manager: EnvManager,
}

impl RequestRunner {
    pub fn new(env_manager: EnvManager) -> Self {
        Self {
            client: Client::new(),
            env_manager,
        }
    }

    pub async fn run_request(&self, req_file: RequestFile, env_profile: &str) -> Result<()> {
        println!("{} using environment: {}", "Executing".green().bold(), env_profile.cyan());

        let method = Method::from_bytes(req_file.method.to_uppercase().as_bytes())
            .map_err(|_| anyhow!("Invalid HTTP method: {}", req_file.method))?;

        // Build Request
        let mut req_builder = self.client.request(method, &req_file.url);

        // Query params
        if let Some(query) = req_file.query {
            req_builder = req_builder.query(&query);
        }

        // Headers
        let mut headers = HeaderMap::new();
        if let Some(req_headers) = req_file.headers {
            for (key, val) in req_headers {
                let name = HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| anyhow!("Invalid header name: {}", key))?;
                let value = HeaderValue::from_str(&val)
                    .map_err(|_| anyhow!("Invalid header value: {}", val))?;
                headers.insert(name, value);
            }
        }
        req_builder = req_builder.headers(headers);

        // Body
        if let Some(body_val) = req_file.body {
            match body_val {
                serde_yaml::Value::String(s) => {
                    req_builder = req_builder.body(s);
                }
                other => {
                    let json_val: JsonValue = serde_yaml::from_value(other)
                        .context("Failed to convert request body to JSON")?;
                    req_builder = req_builder.json(&json_val);
                }
            }
        }

        // Send Request
        let start = std::time::Instant::now();
        let res = req_builder.send().await
            .context("HTTP request failed")?;
        let duration = start.elapsed();

        // Process Response
        self.print_response(res, duration, req_file.exports).await?;

        Ok(())
    }

    async fn print_response(&self, res: Response, duration: std::time::Duration, exports: Option<HashMap<String, String>>) -> Result<()> {
        let status = res.status();
        let status_color = if status.is_success() {
            status.to_string().green()
        } else if status.is_redirection() {
            status.to_string().blue()
        } else {
            status.to_string().red()
        };

        println!("\n{} - {} - {:?}", "Status".bold(), status_color, duration);
        println!("{}", "--- Headers ---".bright_black());
        for (name, value) in res.headers() {
            println!("{}: {}", name.as_str().cyan(), value.to_str().unwrap_or("<binary>"));
        }

        println!("\n{}", "--- Body ---".bright_black());
        
        let content_type = res.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body_bytes = res.bytes().await?;
        let body_str = String::from_utf8_lossy(&body_bytes);

        if content_type.contains("json") {
            if let Ok(json_val) = serde_json::from_str::<JsonValue>(&body_str) {
                let pretty_json = serde_json::to_string_pretty(&json_val)?;
                println!("{}", pretty_json);

                // Handle exports
                if let Some(exp_map) = exports {
                    self.handle_exports(&json_val, exp_map)?;
                }
            } else {
                println!("{}", body_str);
            }
        } else {
            println!("{}", body_str);
        }

        Ok(())
    }

    fn handle_exports(&self, json: &JsonValue, exports: HashMap<String, String>) -> Result<()> {
        for (env_var, json_path) in exports {
            // resolve json_path
            // e.g. "token" or "user.id" or "/token"
            if let Some(val) = self.resolve_json_path(json, &json_path) {
                // convert val to string representation to store in env
                let val_str = match val {
                    JsonValue::String(s) => s.clone(),
                    JsonValue::Number(n) => n.to_string(),
                    JsonValue::Bool(b) => b.to_string(),
                    JsonValue::Null => "null".to_string(),
                    other => serde_json::to_string(other)?,
                };
                self.env_manager.update_active_env_var(&env_var, &val_str)?;
                println!("{} Exported {} = {}", "State:".magenta().bold(), env_var.yellow(), val_str.cyan());
            } else {
                println!("{} Failed to export {}: path '{}' not found in response", "Warning:".yellow().bold(), env_var, json_path);
            }
        }
        Ok(())
    }

    fn resolve_json_path<'a>(&self, json: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
        // Support JSON Pointer (e.g. /data/token) or dot notation (e.g. data.token or token)
        if path.starts_with('/') {
            json.pointer(path)
        } else {
            // Split by '.'
            let parts: Vec<&str> = path.split('.').collect();
            let mut current = json;
            for part in parts {
                current = current.get(part)?;
            }
            Some(current)
        }
    }
}
