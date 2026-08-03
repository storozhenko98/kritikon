use std::{
    env,
    io::{self, IsTerminal, Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use semver::Version;

const REPOSITORY: &str = "storozhenko98/kritikon";
const RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const INSTALLER: &[u8] = include_bytes!("../install.sh");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    Continue,
    RestartRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseInfo {
    tag: String,
    version: Version,
}

pub fn check_and_prompt() -> StartupAction {
    if cfg!(debug_assertions) || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return StartupAction::Continue;
    }

    let Some(release) = latest_newer_release() else {
        return StartupAction::Continue;
    };

    println!();
    println!(
        "Kritikon v{} is available — you are running v{}.",
        release.version,
        env!("CARGO_PKG_VERSION")
    );
    print!("Update now? [y] update / [Enter] not now: ");
    if io::stdout().flush().is_err() {
        return StartupAction::Continue;
    }

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() || !wants_update(&answer) {
        println!("Continuing with Kritikon v{}.", env!("CARGO_PKG_VERSION"));
        return StartupAction::Continue;
    }

    println!("Updating to v{}…", release.version);
    match install_release(&release.tag) {
        Ok(()) => {
            println!();
            println!(
                "Kritikon v{} is installed. Start `kritikon` again to use the new version.",
                release.version
            );
            StartupAction::RestartRequired
        }
        Err(error) => {
            eprintln!("Update failed: {error:#}");
            eprintln!(
                "Continuing with Kritikon v{}. You can retry on the next launch.",
                env!("CARGO_PKG_VERSION")
            );
            StartupAction::Continue
        }
    }
}

fn latest_newer_release() -> Option<ReleaseInfo> {
    let tag = fetch_latest_release_tag().ok()?;
    newer_release(env!("CARGO_PKG_VERSION"), &tag)
}

fn fetch_latest_release_tag() -> Result<String> {
    let mut child = Command::new("gh")
        .args([
            "api",
            &format!("repos/{REPOSITORY}/releases/latest"),
            "--jq",
            ".tag_name",
            "--cache",
            "5m",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start GitHub CLI for update check")?;

    let deadline = Instant::now() + RELEASE_CHECK_TIMEOUT;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("could not poll GitHub release check")?
        {
            if !status.success() {
                bail!("GitHub release check exited with {status}");
            }
            let mut output = String::new();
            child
                .stdout
                .take()
                .context("GitHub release check did not provide output")?
                .read_to_string(&mut output)
                .context("could not read GitHub release response")?;
            let tag = output.trim();
            if tag.is_empty() {
                bail!("GitHub release response did not include a tag");
            }
            return Ok(tag.to_owned());
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("GitHub release check timed out");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn newer_release(current: &str, tag: &str) -> Option<ReleaseInfo> {
    let current = Version::parse(current).ok()?;
    let version_text = tag.strip_prefix('v')?;
    let version = Version::parse(version_text).ok()?;
    (version > current).then(|| ReleaseInfo {
        tag: tag.to_owned(),
        version,
    })
}

fn wants_update(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn install_release(tag: &str) -> Result<()> {
    let executable = env::current_exe().context("could not locate the running executable")?;
    let install_dir = executable
        .parent()
        .context("running executable has no parent directory")?;

    let mut child = Command::new("sh")
        .arg("-s")
        .env("KRITIKON_VERSION", tag)
        .env("KRITIKON_INSTALL_DIR", install_dir)
        .stdin(Stdio::piped())
        .spawn()
        .context("could not start the Kritikon installer")?;

    child
        .stdin
        .take()
        .context("Kritikon installer did not provide stdin")?
        .write_all(INSTALLER)
        .context("could not send the verified installer to the shell")?;

    let status = child
        .wait()
        .context("could not wait for the Kritikon installer")?;
    if !status.success() {
        bail!("installer exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newer_semantic_versions_trigger_an_update() {
        assert_eq!(
            newer_release("0.3.0", "v0.4.0"),
            Some(ReleaseInfo {
                tag: "v0.4.0".into(),
                version: Version::new(0, 4, 0),
            })
        );
        assert!(newer_release("0.3.0", "v0.3.0").is_none());
        assert!(newer_release("0.3.0", "v0.2.9").is_none());
        assert!(newer_release("0.3.0", "release-0.4.0").is_none());
        assert!(newer_release("0.3.0", "not-a-version").is_none());
    }

    #[test]
    fn update_prompt_is_explicit_and_dismissable() {
        assert!(wants_update("y\n"));
        assert!(wants_update("YES"));
        assert!(!wants_update("\n"));
        assert!(!wants_update("n\n"));
        assert!(!wants_update("later"));
    }

    #[test]
    fn embedded_installer_uses_checksums() {
        let installer = String::from_utf8_lossy(INSTALLER);
        assert!(installer.contains("SHA256SUMS"));
        assert!(installer.contains("checksum verification failed"));
    }
}
