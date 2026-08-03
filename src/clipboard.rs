use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

pub fn copy(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        pipe_to("pbcopy", &[], text)
    }

    #[cfg(target_os = "linux")]
    {
        let candidates: [(&str, &[&str]); 3] = [
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        let mut errors = Vec::new();
        for (program, args) in candidates {
            match pipe_to(program, args, text) {
                Ok(()) => return Ok(()),
                Err(error) => errors.push(format!("{program}: {error:#}")),
            }
        }
        anyhow::bail!(
            "no Linux clipboard utility succeeded; install wl-copy, xclip, or xsel ({})",
            errors.join("; ")
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = text;
        anyhow::bail!("clipboard copying is supported on macOS and Linux only");
    }
}

fn pipe_to(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {program}"))?;
    child
        .stdin
        .take()
        .context("clipboard process did not provide stdin")?
        .write_all(text.as_bytes())
        .with_context(|| format!("could not write to {program}"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("could not wait for {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            anyhow::bail!("{program} exited with {}", output.status);
        }
        anyhow::bail!("{stderr}");
    }
    Ok(())
}
