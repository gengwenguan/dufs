use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::Mutex;
use uuid::Uuid;

pub const DIRECTORY_PASSWORD_QUERY: &str = "dir_password";
const STORE_VERSION: u8 = 1;
const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 128;

#[derive(Debug, Default, Deserialize, Serialize)]
struct DirectorySecrets {
    version: u8,
    directories: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct DirectoryAuth {
    root: PathBuf,
    file: PathBuf,
    secrets: Mutex<DirectorySecrets>,
}

impl DirectoryAuth {
    pub fn load(root: PathBuf, file: PathBuf) -> Result<Self> {
        let secrets = if file.exists() {
            let content = std::fs::read(&file)
                .with_context(|| format!("Failed to read {}", file.display()))?;
            let secrets: DirectorySecrets = serde_json::from_slice(&content)
                .with_context(|| format!("Failed to parse {}", file.display()))?;
            if secrets.version != STORE_VERSION {
                bail!(
                    "Unsupported directory auth file version {}",
                    secrets.version
                );
            }
            for (path, password) in &secrets.directories {
                validate_directory_path(path)?;
                validate_password(password)?;
            }
            secrets
        } else {
            DirectorySecrets {
                version: STORE_VERSION,
                ..Default::default()
            }
        };
        Ok(Self {
            root,
            file,
            secrets: Mutex::new(secrets),
        })
    }

    pub fn metadata_path(&self) -> &Path {
        &self.file
    }

    pub async fn password_for_target(
        &self,
        relative_path: &str,
        absolute_path: &Path,
    ) -> Result<Option<String>> {
        Ok(self
            .grant_for_target(relative_path, absolute_path)
            .await?
            .map(|(_, password)| password))
    }

    pub async fn grant_for_target(
        &self,
        relative_path: &str,
        absolute_path: &Path,
    ) -> Result<Option<(String, String)>> {
        let Some(directory) = self
            .protecting_directory(relative_path, absolute_path)
            .await
        else {
            return Ok(None);
        };
        let password = self.password_for_directory(&directory).await?;
        Ok(Some((directory, password)))
    }

    pub async fn password_for_directory(&self, relative_path: &str) -> Result<String> {
        validate_directory_path(relative_path)?;
        let mut secrets = self.secrets.lock().await;
        if let Some(password) = secrets.directories.get(relative_path) {
            return Ok(password.clone());
        }
        let password = random_password();
        secrets
            .directories
            .insert(relative_path.to_string(), password.clone());
        self.persist(&secrets).await?;
        Ok(password)
    }

    pub async fn registered_password(&self, relative_path: &str) -> Option<String> {
        self.secrets
            .lock()
            .await
            .directories
            .get(relative_path)
            .cloned()
    }

    pub async fn ensure_directory_tree(&self, relative_path: &str) -> Result<()> {
        let mut directories = Vec::new();
        let mut current = String::new();
        for part in relative_path.split('/').filter(|part| !part.is_empty()) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            if fs::metadata(self.root.join(&current))
                .await
                .map(|meta| meta.is_dir())
                .unwrap_or(false)
            {
                directories.push(current.clone());
            }
        }
        if directories.is_empty() {
            return Ok(());
        }

        let mut secrets = self.secrets.lock().await;
        let mut changed = false;
        for directory in directories {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                secrets.directories.entry(directory)
            {
                entry.insert(random_password());
                changed = true;
            }
        }
        if changed {
            self.persist(&secrets).await?;
        }
        Ok(())
    }

    pub async fn set_password(&self, relative_path: &str, password: &str) -> Result<()> {
        validate_directory_path(relative_path)?;
        validate_password(password)?;
        let mut secrets = self.secrets.lock().await;
        secrets
            .directories
            .insert(relative_path.to_string(), password.to_string());
        self.persist(&secrets).await
    }

    pub async fn remove_tree(&self, relative_path: &str) -> Result<()> {
        validate_directory_path(relative_path)?;
        let prefix = format!("{relative_path}/");
        let mut secrets = self.secrets.lock().await;
        let old_len = secrets.directories.len();
        secrets
            .directories
            .retain(|path, _| path != relative_path && !path.starts_with(&prefix));
        if secrets.directories.len() != old_len {
            self.persist(&secrets).await?;
        }
        Ok(())
    }

    pub async fn move_tree(&self, source: &str, destination: &str) -> Result<()> {
        validate_directory_path(source)?;
        validate_directory_path(destination)?;
        let source_prefix = format!("{source}/");
        let destination_prefix = format!("{destination}/");
        let mut secrets = self.secrets.lock().await;
        let moved: Vec<(String, String)> = secrets
            .directories
            .iter()
            .filter_map(|(path, password)| {
                if path == source {
                    Some((destination.to_string(), password.clone()))
                } else {
                    path.strip_prefix(&source_prefix)
                        .map(|suffix| (format!("{destination_prefix}{suffix}"), password.clone()))
                }
            })
            .collect();
        if moved.is_empty() {
            return Ok(());
        }
        secrets.directories.retain(|path, _| {
            path != source
                && !path.starts_with(&source_prefix)
                && path != destination
                && !path.starts_with(&destination_prefix)
        });
        secrets.directories.extend(moved);
        self.persist(&secrets).await
    }

    async fn protecting_directory(
        &self,
        relative_path: &str,
        absolute_path: &Path,
    ) -> Option<String> {
        let mut parts: Vec<&str> = relative_path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let mut path = absolute_path.to_path_buf();

        loop {
            if fs::metadata(&path)
                .await
                .map(|meta| meta.is_dir())
                .unwrap_or(false)
            {
                return (!parts.is_empty()).then(|| parts.join("/"));
            }
            if parts.pop().is_none() || !path.pop() {
                return None;
            }
        }
    }

    async fn persist(&self, secrets: &DirectorySecrets) -> Result<()> {
        let parent = self
            .file
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid directory auth file path"))?;
        fs::create_dir_all(parent).await?;
        let filename = self
            .file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("directory-auth.json");
        let temp = self
            .file
            .with_file_name(format!("{filename}.tmp-{}", Uuid::new_v4().simple()));
        let content = serde_json::to_vec_pretty(secrets)?;
        fs::write(&temp, content).await?;
        set_private_permissions(&temp).await?;
        if let Err(err) = fs::rename(&temp, &self.file).await {
            let _ = fs::remove_file(&temp).await;
            return Err(err.into());
        }
        set_private_permissions(&self.file).await?;
        Ok(())
    }
}

pub fn password_matches(expected: &str, candidate: Option<&String>) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let expected = expected.as_bytes();
    let candidate = candidate.as_bytes();
    let mut difference = expected.len() ^ candidate.len();
    let max_len = expected.len().max(candidate.len());
    for index in 0..max_len {
        let left = expected.get(index).copied().unwrap_or_default();
        let right = candidate.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn random_password() -> String {
    Uuid::new_v4().simple().to_string()
}

fn validate_directory_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("Invalid directory auth path");
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<()> {
    if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&password.len()) {
        bail!(
            "Directory password must contain between {MIN_PASSWORD_LEN} and {MAX_PASSWORD_LEN} bytes"
        );
    }
    if password.chars().any(char::is_control) {
        bail!("Directory password cannot contain control characters");
    }
    Ok(())
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
