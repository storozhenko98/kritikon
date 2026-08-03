use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u8 = 2;
pub const DEFAULT_REFRESH_SECONDS: u64 = 30;
pub const MIN_REFRESH_SECONDS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u8,
    pub refresh_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            refresh_seconds: DEFAULT_REFRESH_SECONDS,
        }
    }
}

impl Config {
    pub fn new(refresh_seconds: u64) -> Result<Self> {
        let config = Self {
            version: CONFIG_VERSION,
            refresh_seconds,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported config version {}; expected {CONFIG_VERSION}",
                self.version
            );
        }
        if self.refresh_seconds < MIN_REFRESH_SECONDS {
            bail!(
                "refresh_seconds must be an integer of at least {MIN_REFRESH_SECONDS}; got {}",
                self.refresh_seconds
            );
        }
        Ok(())
    }
}

pub fn parse_refresh_seconds(value: &str) -> Result<u64, String> {
    let seconds = value.parse::<u64>().map_err(|_| {
        format!(
            "refresh interval must be a whole number of seconds (minimum {MIN_REFRESH_SECONDS})"
        )
    })?;
    if seconds < MIN_REFRESH_SECONDS {
        return Err(format!(
            "refresh interval must be at least {MIN_REFRESH_SECONDS} seconds"
        ));
    }
    Ok(seconds)
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn system() -> Result<Self> {
        Ok(Self {
            path: default_config_path()?,
        })
    }

    #[cfg(any(debug_assertions, test))]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Config> {
        let source = fs::read_to_string(&self.path)
            .with_context(|| format!("could not read {}", self.path.display()))?;
        let config = match toml::from_str::<Config>(&source) {
            Ok(config) => config,
            Err(current_error) => match toml::from_str::<LegacyConfig>(&source) {
                Ok(legacy) if legacy.version == 1 => Config::new(legacy.refresh_seconds)
                    .with_context(|| format!("invalid config in {}", self.path.display()))?,
                _ => {
                    return Err(current_error)
                        .with_context(|| format!("invalid config in {}", self.path.display()));
                }
            },
        };
        config
            .validate()
            .with_context(|| format!("invalid config in {}", self.path.display()))?;
        Ok(config)
    }

    pub fn load_or_default(&self) -> Result<Config> {
        match self.load() {
            Ok(config) => Ok(config),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(Config::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        config.validate()?;
        let parent = self
            .path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;

        let contents = toml::to_string_pretty(config).context("could not serialize config")?;
        let temporary = self
            .path
            .with_extension(format!("toml.tmp-{}", std::process::id()));
        let result = write_private_file(&temporary, contents.as_bytes()).and_then(|()| {
            fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "could not atomically replace configuration at {}",
                    self.path.display()
                )
            })
        });
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn reset(&self) -> Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("could not delete {}", self.path.display()))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyConfig {
    version: u8,
    refresh_seconds: u64,
    #[serde(rename = "review_scope")]
    _review_scope: LegacyReviewScope,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyReviewScope {
    All,
    Direct,
    Teams,
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("could not sync {}", path.display()))
}

#[cfg(not(unix))]
fn write_private_file(_path: &Path, _contents: &[u8]) -> Result<()> {
    bail!("Kritikon supports configuration on macOS and Linux only")
}

fn default_config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot determine configuration directory")?;

    #[cfg(target_os = "macos")]
    {
        Ok(config_path_for(
            Platform::MacOs,
            &home,
            env::var_os("XDG_CONFIG_HOME").as_deref().map(Path::new),
        ))
    }

    #[cfg(target_os = "linux")]
    {
        Ok(config_path_for(
            Platform::Linux,
            &home,
            env::var_os("XDG_CONFIG_HOME").as_deref().map(Path::new),
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        bail!("Kritikon supports configuration on macOS and Linux only")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum Platform {
    MacOs,
    Linux,
}

fn config_path_for(platform: Platform, home: &Path, xdg: Option<&Path>) -> PathBuf {
    let base = match platform {
        Platform::MacOs => home.join("Library").join("Application Support"),
        Platform::Linux => xdg
            .filter(|path| path.is_absolute())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config")),
    };
    base.join("kritikon").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_thirty_seconds() {
        let config = Config::default();
        assert_eq!(config.refresh_seconds, 30);
        config.validate().unwrap();
    }

    #[test]
    fn refresh_validation_has_a_minimum_and_no_artificial_maximum() {
        assert!(Config::new(4).is_err());
        assert!(Config::new(5).is_ok());
        assert!(Config::new(u64::MAX).is_ok());
        assert!(parse_refresh_seconds("5s").is_err());
        assert!(parse_refresh_seconds("5.5").is_err());
    }

    #[test]
    fn uses_native_macos_and_linux_paths() {
        let home = Path::new("/Users/alice");
        assert_eq!(
            config_path_for(Platform::MacOs, home, None),
            Path::new("/Users/alice/Library/Application Support/kritikon/config.toml")
        );

        let linux_home = Path::new("/home/alice");
        assert_eq!(
            config_path_for(Platform::Linux, linux_home, None),
            Path::new("/home/alice/.config/kritikon/config.toml")
        );
        assert_eq!(
            config_path_for(Platform::Linux, linux_home, Some(Path::new("/var/config"))),
            Path::new("/var/config/kritikon/config.toml")
        );
        assert_eq!(
            config_path_for(Platform::Linux, linux_home, Some(Path::new("relative"))),
            Path::new("/home/alice/.config/kritikon/config.toml")
        );
    }

    #[test]
    fn saves_loads_and_resets_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("nested/config.toml"));
        let config = Config::new(60).unwrap();

        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(store.reset().unwrap());
        assert!(!store.reset().unwrap());
        assert_eq!(store.load_or_default().unwrap(), Config::default());
    }

    #[test]
    fn malformed_and_unknown_configuration_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        fs::write(store.path(), "version = 2\nrefresh_seconds = 4\n").unwrap();
        assert!(store.load().is_err());

        fs::write(
            store.path(),
            "version = 2\nrefresh_seconds = 30\nreview_scope = \"friends\"\n",
        )
        .unwrap();
        assert!(store.load().is_err());

        fs::write(
            store.path(),
            "version = 2\nrefresh_seconds = 30\nextra = true\n",
        )
        .unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn migrates_the_removed_scope_setting_from_version_one() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("config.toml"));
        fs::write(
            store.path(),
            "version = 1\nrefresh_seconds = 60\nreview_scope = \"teams\"\n",
        )
        .unwrap();

        assert_eq!(store.load().unwrap(), Config::new(60).unwrap());
    }
}
