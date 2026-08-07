use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Result, bail};

pub fn copy(text: &str) -> Result<()> {
    let mut failures = Vec::new();
    for (program, arguments) in clipboard_commands() {
        match copy_with(program, arguments, text) {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }

    if failures.is_empty() {
        bail!("clipboard is not supported on this platform");
    }
    bail!("could not copy to clipboard ({})", failures.join("; "))
}

fn copy_with(program: &str, arguments: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("clipboard command stdin is piped")
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clipboard_commands() -> Vec<(&'static str, &'static [&'static str])> {
    vec![("pbcopy", &[])]
}

#[cfg(target_os = "linux")]
fn clipboard_commands() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ]
}

#[cfg(target_os = "windows")]
fn clipboard_commands() -> Vec<(&'static str, &'static [&'static str])> {
    vec![("clip.exe", &[])]
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn clipboard_commands() -> Vec<(&'static str, &'static [&'static str])> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    fn supported_platform_has_a_clipboard_command() {
        assert!(!clipboard_commands().is_empty());
    }
}
