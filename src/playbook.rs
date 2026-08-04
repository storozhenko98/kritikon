use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::write_private_file;

pub const PLAYBOOK_FILE_VERSION: u8 = 1;
pub const MAX_CUSTOM_PLAYBOOKS: usize = 50;
pub const MAX_PLAYBOOK_NAME_CHARS: usize = 48;
pub const MAX_PLAYBOOK_PROMPT_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPlaybook {
    pub name: String,
    pub prompt: String,
    pub built_in: bool,
}

impl ReviewPlaybook {
    pub fn custom(name: impl Into<String>, prompt: impl Into<String>) -> Result<Self> {
        let playbook = Self {
            name: name.into().trim().to_string(),
            prompt: prompt.into().trim().to_string(),
            built_in: false,
        };
        validate_name(&playbook.name)?;
        validate_prompt(&playbook.prompt)?;
        Ok(playbook)
    }

    fn built_in(name: &str, prompt: &str) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
            built_in: true,
        }
    }
}

pub fn built_in_playbooks() -> Vec<ReviewPlaybook> {
    vec![
        ReviewPlaybook::built_in(
            "Security & authorization",
            "Trace authentication and authorization boundaries, privilege escalation, tenant isolation, indirect object access, secret exposure, and unsafe trust assumptions. Explain concrete exploit paths rather than listing generic concerns.",
        ),
        ReviewPlaybook::built_in(
            "Database migrations",
            "Review schema and data migrations for reversibility, locking, long transactions, unsafe backfills, constraint ordering, partial rollout behavior, and compatibility while old and new application versions run together.",
        ),
        ReviewPlaybook::built_in(
            "API compatibility",
            "Look for breaking request, response, validation, serialization, pagination, error, and versioning changes. Trace effects through callers and identify compatibility failures with concrete examples.",
        ),
        ReviewPlaybook::built_in(
            "Performance & concurrency",
            "Inspect hot paths for unbounded work, N+1 queries, repeated I/O, excessive allocation, lock contention, races, deadlocks, retry storms, and missing cancellation or backpressure.",
        ),
    ]
}

pub fn catalog(custom: &[ReviewPlaybook]) -> Vec<ReviewPlaybook> {
    let mut playbooks = built_in_playbooks();
    playbooks.extend(custom.iter().cloned());
    playbooks
}

pub fn validate_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("playbook name cannot be empty");
    }
    if trimmed.chars().count() > MAX_PLAYBOOK_NAME_CHARS {
        bail!("playbook name must be at most {MAX_PLAYBOOK_NAME_CHARS} characters");
    }
    if trimmed.chars().any(char::is_control) {
        bail!("playbook name cannot contain control characters");
    }
    Ok(())
}

pub fn validate_prompt(prompt: &str) -> Result<()> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        bail!("playbook instructions cannot be empty");
    }
    if trimmed.chars().count() > MAX_PLAYBOOK_PROMPT_CHARS {
        bail!("playbook instructions must be at most {MAX_PLAYBOOK_PROMPT_CHARS} characters");
    }
    if trimmed
        .chars()
        .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        bail!("playbook instructions contain an unsupported control character");
    }
    Ok(())
}

pub fn validate_custom_playbooks(playbooks: &[ReviewPlaybook]) -> Result<()> {
    if playbooks.len() > MAX_CUSTOM_PLAYBOOKS {
        bail!("at most {MAX_CUSTOM_PLAYBOOKS} custom playbooks can be saved");
    }
    let mut names = built_in_playbooks()
        .into_iter()
        .map(|playbook| playbook.name.to_lowercase())
        .collect::<HashSet<_>>();
    for playbook in playbooks {
        if playbook.built_in {
            bail!("built-in playbooks cannot be written to the custom playbook file");
        }
        validate_name(&playbook.name)?;
        validate_prompt(&playbook.prompt)?;
        if !names.insert(playbook.name.to_lowercase()) {
            bail!(
                "playbook names must be unique; duplicate: {}",
                playbook.name
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PlaybookStore {
    path: PathBuf,
}

impl PlaybookStore {
    pub fn beside_config(config_path: &Path) -> Result<Self> {
        let parent = config_path
            .parent()
            .context("configuration path has no parent directory")?;
        Ok(Self {
            path: parent.join("playbooks.toml"),
        })
    }

    #[cfg(test)]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<ReviewPlaybook>> {
        let source = fs::read_to_string(&self.path)
            .with_context(|| format!("could not read {}", self.path.display()))?;
        let file = toml::from_str::<PlaybookFile>(&source)
            .with_context(|| format!("invalid playbook file in {}", self.path.display()))?;
        if file.version != PLAYBOOK_FILE_VERSION {
            bail!(
                "unsupported playbook file version {}; expected {PLAYBOOK_FILE_VERSION}",
                file.version
            );
        }
        let playbooks = file
            .playbooks
            .into_iter()
            .map(|stored| ReviewPlaybook::custom(stored.name, stored.prompt))
            .collect::<Result<Vec<_>>>()?;
        validate_custom_playbooks(&playbooks)?;
        Ok(playbooks)
    }

    pub fn load_or_default(&self) -> Result<Vec<ReviewPlaybook>> {
        match self.load() {
            Ok(playbooks) => Ok(playbooks),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, playbooks: &[ReviewPlaybook]) -> Result<()> {
        validate_custom_playbooks(playbooks)?;
        let parent = self
            .path
            .parent()
            .context("playbook path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let file = PlaybookFile {
            version: PLAYBOOK_FILE_VERSION,
            playbooks: playbooks
                .iter()
                .map(|playbook| StoredPlaybook {
                    name: playbook.name.clone(),
                    prompt: playbook.prompt.clone(),
                })
                .collect(),
        };
        let contents = toml::to_string_pretty(&file).context("could not serialize playbooks")?;
        let temporary = self
            .path
            .with_extension(format!("toml.tmp-{}", std::process::id()));
        let result = write_private_file(&temporary, contents.as_bytes()).and_then(|()| {
            fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "could not atomically replace playbooks at {}",
                    self.path.display()
                )
            })
        });
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaybookFile {
    version: u8,
    #[serde(default)]
    playbooks: Vec<StoredPlaybook>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPlaybook {
    name: String,
    prompt: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names_prompts_limits_and_uniqueness() {
        assert!(ReviewPlaybook::custom("", "prompt").is_err());
        assert!(ReviewPlaybook::custom("name", "").is_err());
        assert!(ReviewPlaybook::custom("x".repeat(49), "prompt").is_err());
        assert!(ReviewPlaybook::custom("name", "x".repeat(8_001)).is_err());
        assert!(ReviewPlaybook::custom("name", "unsafe\u{1b}escape").is_err());

        let duplicate_builtin = ReviewPlaybook::custom("security & AUTHORIZATION", "mine").unwrap();
        assert!(validate_custom_playbooks(&[duplicate_builtin]).is_err());
        let first = ReviewPlaybook::custom("Release safety", "Check rollout safety.").unwrap();
        let duplicate = ReviewPlaybook::custom("release SAFETY", "Check flags.").unwrap();
        assert!(validate_custom_playbooks(&[first, duplicate]).is_err());

        let too_many = (0..=MAX_CUSTOM_PLAYBOOKS)
            .map(|index| ReviewPlaybook::custom(format!("Playbook {index}"), "Review it.").unwrap())
            .collect::<Vec<_>>();
        assert!(validate_custom_playbooks(&too_many).is_err());
    }

    #[test]
    fn saves_loads_and_keeps_playbooks_separate_from_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("nested/config.toml");
        let store = PlaybookStore::beside_config(&config_path).unwrap();
        assert_eq!(store.path(), directory.path().join("nested/playbooks.toml"));
        assert!(store.load_or_default().unwrap().is_empty());

        let playbooks = vec![
            ReviewPlaybook::custom(
                "Release safety",
                "Check feature flags, rollback behavior, and mixed-version deployments.",
            )
            .unwrap(),
        ];
        store.save(&playbooks).unwrap();
        assert_eq!(store.load().unwrap(), playbooks);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let config_store = crate::config::ConfigStore::at(config_path);
        config_store
            .save(&crate::config::Config::default())
            .unwrap();
        assert!(config_store.reset().unwrap());
        assert_eq!(store.load().unwrap(), playbooks);
    }

    #[test]
    fn malformed_unknown_and_builtin_entries_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = PlaybookStore::at(directory.path().join("playbooks.toml"));
        fs::write(store.path(), "version = 2\nplaybooks = []\n").unwrap();
        assert!(store.load().is_err());
        fs::write(store.path(), "version = 1\nextra = true\nplaybooks = []\n").unwrap();
        assert!(store.load().is_err());
        fs::write(
            store.path(),
            "version = 1\n[[playbooks]]\nname = \"Security & authorization\"\nprompt = \"replace\"\n",
        )
        .unwrap();
        assert!(store.load().is_err());
    }
}
