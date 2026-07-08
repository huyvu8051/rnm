use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use directories::ProjectDirs;

pub const APP_NAME: &str = "rnm";

#[derive(Clone)]
pub struct EnvManager {
    config_dir: PathBuf,
}

impl EnvManager {
    pub fn new() -> Result<Self> {
        // Prefer local directory .rnm if it exists, otherwise use standard config dir
        let local_dir = Path::new(".rnm");
        let config_dir = if local_dir.is_dir() {
            local_dir.to_path_buf()
        } else if let Some(proj_dirs) = ProjectDirs::from("", "", APP_NAME) {
            proj_dirs.config_dir().to_path_buf()
        } else {
            PathBuf::from(".rnm")
        };

        // Ensure directories exist
        fs::create_dir_all(config_dir.join("env"))?;

        Ok(Self { config_dir })
    }

    fn get_active_env_path(&self) -> PathBuf {
        self.config_dir.join("active_env")
    }

    fn get_env_dir(&self) -> PathBuf {
        self.config_dir.join("env")
    }

    pub fn get_active_env_name(&self) -> Result<Option<String>> {
        let path = self.get_active_env_path();
        if path.exists() {
            let content = fs::read_to_string(path)?;
            let name = content.trim().to_string();
            if name.is_empty() {
                Ok(None)
            } else {
                Ok(Some(name))
            }
        } else {
            Ok(None)
        }
    }

    pub fn set_active_env(&self, name: &str) -> Result<()> {
        let env_path = self.get_env_dir().join(format!("{}.yaml", name));
        if !env_path.exists() {
            // Create empty env if it doesn't exist
            let empty: HashMap<String, String> = HashMap::new();
            let yaml = serde_yaml::to_string(&empty)?;
            fs::write(&env_path, yaml)?;
        }
        
        fs::write(self.get_active_env_path(), name.trim())?;
        Ok(())
    }

    pub fn load_active_env(&self) -> Result<HashMap<String, String>> {
        let name_opt = self.get_active_env_name()?;
        match name_opt {
            Some(name) => self.load_env(&name),
            None => Ok(HashMap::new()),
        }
    }

    pub fn load_env(&self, name: &str) -> Result<HashMap<String, String>> {
        let env_path = self.get_env_dir().join(format!("{}.yaml", name));
        if !env_path.exists() {
            return Ok(HashMap::new());
        }

        let content = fs::read_to_string(env_path)?;
        let env: HashMap<String, String> = serde_yaml::from_str(&content)
            .context("Failed to parse environment YAML file")?;
        Ok(env)
    }

    pub fn save_env(&self, name: &str, env: &HashMap<String, String>) -> Result<()> {
        let env_path = self.get_env_dir().join(format!("{}.yaml", name));
        let yaml = serde_yaml::to_string(env)?;
        fs::write(env_path, yaml)?;
        Ok(())
    }

    pub fn update_active_env_var(&self, key: &str, value: &str) -> Result<()> {
        if let Some(name) = self.get_active_env_name()? {
            let mut env = self.load_env(&name)?;
            env.insert(key.to_string(), value.to_string());
            self.save_env(&name, &env)?;
        } else {
            // Fallback or create 'default' env
            self.set_active_env("default")?;
            let mut env = self.load_env("default")?;
            env.insert(key.to_string(), value.to_string());
            self.save_env("default", &env)?;
        }
        Ok(())
    }

    pub fn list_envs(&self) -> Result<Vec<String>> {
        let mut envs = Vec::new();
        let env_dir = self.get_env_dir();
        if env_dir.exists() {
            for entry in fs::read_dir(env_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("yaml") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        envs.push(name.to_string());
                    }
                }
            }
        }
        Ok(envs)
    }

    pub fn replace_variables(&self, text: &str, env: &HashMap<String, String>) -> String {
        let mut result = text.to_string();
        for (key, val) in env {
            let placeholder = format!("{{{{{}}}}}", key);
            result = result.replace(&placeholder, val);
        }
        result
    }
}
