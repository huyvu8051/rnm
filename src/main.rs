use clap::{Parser, Subcommand};
use anyhow::{anyhow, Result, Context};
use colored::*;
use std::collections::HashMap;

mod env;
mod request;

use env::EnvManager;
use request::{RequestRunner, RequestFile};

#[derive(Parser)]
#[command(name = "rnm")]
#[command(about = "Rust Network Manager - A curl-like HTTP client with postman-like environment states for AI Agents", long_about = None)]
struct Cli {
    /// URL to request (e.g. https://httpbin.org/get)
    url: Option<String>,

    /// HTTP method (GET, POST, PUT, DELETE, etc.)
    #[arg(short = 'X', long = "request")]
    method: Option<String>,

    /// Headers to include (e.g. -H "Authorization: Bearer {{token}}")
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,

    /// Request body / data (e.g. -d '{"username": "admin"}')
    #[arg(short = 'd', long = "data")]
    data: Option<String>,

    /// Temporarily override active environment for this request
    #[arg(short = 'e', long = "env")]
    env: Option<String>,

    /// Export values from response JSON to environment (e.g. --export token=json.token)
    #[arg(long = "export")]
    exports: Vec<String>,

    /// Path to a request YAML file instead of specifying inline arguments
    #[arg(short = 'f', long = "file")]
    file: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage environment states
    Env {
        #[command(subcommand)]
        subcommand: EnvCommands,
    },
}

#[derive(Subcommand)]
enum EnvCommands {
    /// Set a variable in the active environment (e.g. rnm env set baseUrl https://api.com)
    Set {
        key: String,
        value: String,
    },
    /// Kích hoạt môi trường (e.g. rnm env use dev)
    Use {
        name: String,
    },
    /// Liệt kê các môi trường
    List,
    /// Hiển thị môi trường hiện tại và các biến của nó
    Show,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let env_manager = EnvManager::new()?;

    // If an environment command is run
    if let Some(Commands::Env { subcommand }) = cli.command {
        match subcommand {
            EnvCommands::Set { key, value } => {
                env_manager.update_active_env_var(&key, &value)?;
                let active = env_manager.get_active_env_name()?.unwrap_or_else(|| "default".to_string());
                println!("Set {} = {} in environment: {}", key.cyan(), value.cyan(), active.green());
            }
            EnvCommands::Use { name } => {
                env_manager.set_active_env(&name)?;
                println!("Switched to environment: {}", name.green().bold());
            }
            EnvCommands::List => {
                let envs = env_manager.list_envs()?;
                let active = env_manager.get_active_env_name()?;
                println!("{}", "Available Environments:".bold());
                for env in envs {
                    if Some(&env) == active.as_ref() {
                        println!("* {} (active)", env.green().bold());
                    } else {
                        println!("  {}", env);
                    }
                }
            }
            EnvCommands::Show => {
                let active = env_manager.get_active_env_name()?;
                match active {
                    Some(name) => {
                        println!("Active environment: {}", name.green().bold());
                        let vars = env_manager.load_env(&name)?;
                        if vars.is_empty() {
                            println!("No variables defined.");
                        } else {
                            for (k, v) in vars {
                                println!("  {} = {}", k.cyan(), v);
                            }
                        }
                    }
                    None => {
                        println!("No active environment. Use 'rnm env use <name>' to activate one.");
                    }
                }
            }
        }
        return Ok(());
    }

    // Otherwise, we are executing a request
    let runner = RequestRunner::new(env_manager.clone());

    // 1. Determine active environment to use
    let env_profile = if let Some(ref env_override) = cli.env {
        env_override.clone()
    } else {
        env_manager.get_active_env_name()?.unwrap_or_else(|| "default".to_string())
    };
    let env_vars = env_manager.load_env(&env_profile)?;

    // 2. Build RequestFile properties either from file or command-line args
    let req_file = if let Some(ref file_path) = cli.file {
        let file_content = std::fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read request file: {}", file_path))?;
        // Interpolate templates
        let interpolated = env_manager.replace_variables(&file_content, &env_vars);
        serde_yaml::from_str::<RequestFile>(&interpolated)?
    } else if let Some(ref url) = cli.url {
        // Interpolate templates in CLI inputs
        let url_interpolated = env_manager.replace_variables(url, &env_vars);
        let method = cli.method.unwrap_or_else(|| "GET".to_string()).to_uppercase();

        let mut headers = HashMap::new();
        for h in cli.headers {
            let h_interpolated = env_manager.replace_variables(&h, &env_vars);
            if let Some((k, v)) = h_interpolated.split_once(':') {
                headers.insert(k.trim().to_string(), v.trim().to_string());
            } else {
                return Err(anyhow!("Invalid header format: {}. Must be Key: Value", h));
            }
        }

        let body = cli.data.map(|d| {
            let d_interpolated = env_manager.replace_variables(&d, &env_vars);
            if let Ok(yaml_val) = serde_yaml::from_str::<serde_yaml::Value>(&d_interpolated) {
                yaml_val
            } else {
                serde_yaml::Value::String(d_interpolated)
            }
        });

        let mut exports = HashMap::new();
        for exp in cli.exports {
            if let Some((k, v)) = exp.split_once('=') {
                exports.insert(k.trim().to_string(), v.trim().to_string());
            } else {
                return Err(anyhow!("Invalid export format: {}. Must be Key=JsonPath", exp));
            }
        }

        RequestFile {
            name: None,
            method,
            url: url_interpolated,
            headers: Some(headers),
            query: None,
            body,
            exports: Some(exports),
        }
    } else {
        return Err(anyhow!("Missing URL or request file (-f/--file). Run with --help for usage details."));
    };

    // 3. Execute request using runner
    // We already resolved templates, so we can convert req_file to be run directly by the runner.
    // Let's modify request runner to accept RequestFile directly instead of just reading from file.
    if let Err(e) = runner.run_request(req_file, &env_profile).await {
        eprintln!("{} {}", "Error:".red().bold(), e);
        std::process::exit(1);
    }

    Ok(())
}
