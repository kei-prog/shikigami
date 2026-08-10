use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};

static NEXT_CLIPBOARD_FILE: AtomicU64 = AtomicU64::new(0);

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

#[cfg(target_os = "macos")]
pub fn paste_image_to_temp_png() -> Result<PathBuf> {
    const SCRIPT: &str = r#"on run argv
set outputPath to item 1 of argv
set imageData to the clipboard as «class PNGf»
set outputFile to open for access POSIX file outputPath with write permission
try
  set eof outputFile to 0
  write imageData to outputFile
  close access outputFile
on error message
  try
    close access outputFile
  end try
  error message
end try
end run"#;
    let path = reserve_temp_png()?;
    let output = Command::new("osascript")
        .args(["-e", SCRIPT])
        .arg(&path)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&path);
            return Err(error).context("run osascript");
        }
    };
    finish_clipboard_image(path, output.status.success(), &output.stderr)
}

#[cfg(target_os = "linux")]
pub fn paste_image_to_temp_png() -> Result<PathBuf> {
    let mut failures = Vec::new();
    for (program, arguments) in [
        ("wl-paste", &["--no-newline", "--type", "image/png"][..]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"][..],
        ),
    ] {
        match Command::new(program).args(arguments).output() {
            Ok(output) if output.status.success() => {
                let path = reserve_temp_png()?;
                fs::write(&path, output.stdout).context("write clipboard image")?;
                return finish_clipboard_image(path, true, &output.stderr);
            }
            Ok(output) => failures.push(format!(
                "{program}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    bail!(
        "clipboard does not contain a PNG image ({})",
        failures.join("; ")
    )
}

#[cfg(target_os = "windows")]
pub fn paste_image_to_temp_png() -> Result<PathBuf> {
    const SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $image = [Windows.Forms.Clipboard]::GetImage(); if ($null -eq $image) { exit 1 }; $image.Save($args[0], [Drawing.Imaging.ImageFormat]::Png)"#;
    let path = reserve_temp_png()?;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", SCRIPT])
        .arg(&path)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&path);
            return Err(error).context("run powershell.exe");
        }
    };
    finish_clipboard_image(path, output.status.success(), &output.stderr)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn paste_image_to_temp_png() -> Result<PathBuf> {
    bail!("clipboard image paste is not supported on this platform")
}

fn reserve_temp_png() -> Result<PathBuf> {
    for _ in 0..100 {
        let sequence = NEXT_CLIPBOARD_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "shikigami-clipboard-{}-{sequence}.png",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create clipboard image file"),
        }
    }
    bail!("could not reserve a clipboard image file")
}

fn finish_clipboard_image(path: PathBuf, success: bool, stderr: &[u8]) -> Result<PathBuf> {
    if success && is_supported_image(&path) {
        return Ok(path);
    }
    let _ = fs::remove_file(&path);
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        bail!("clipboard does not contain an image")
    }
    bail!("clipboard does not contain an image: {detail}")
}

/// Return a pasted single path only when it names a readable supported image.
pub fn pasted_image_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();
    let unquoted = pasted
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            pasted
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(pasted);
    let literal_path = PathBuf::from(unquoted);
    if is_supported_image(&literal_path) {
        return Some(literal_path);
    }
    #[cfg(target_os = "windows")]
    if unquoted == pasted {
        let path = PathBuf::from(pasted.replace("\\ ", " "));
        if is_supported_image(&path) {
            return Some(path);
        }
    }
    let path = if unquoted == pasted {
        let mut parts = shlex::Shlex::new(pasted);
        let path = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        PathBuf::from(path)
    } else {
        PathBuf::from(unquoted)
    };
    is_supported_image(&path).then_some(path)
}

fn is_supported_image(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 12];
    let Ok(length) = file.read(&mut header) else {
        return false;
    };
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => length >= 8 && header[..8] == *b"\x89PNG\r\n\x1a\n",
        Some("jpg" | "jpeg") => length >= 3 && header[..3] == *b"\xff\xd8\xff",
        Some("gif") => length >= 6 && matches!(&header[..6], b"GIF87a" | b"GIF89a"),
        Some("webp") => length >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP",
        _ => false,
    }
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
    use tempfile::tempdir;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    fn supported_platform_has_a_clipboard_command() {
        assert!(!clipboard_commands().is_empty());
    }

    #[test]
    fn recognizes_plain_quoted_and_shell_escaped_image_paths() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("pasted image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nheader").unwrap();

        assert_eq!(
            pasted_image_path(path.to_str().unwrap()),
            Some(path.clone())
        );
        assert_eq!(
            pasted_image_path(&format!("\"{}\"", path.display())),
            Some(path.clone())
        );
        assert_eq!(
            pasted_image_path(&path.to_string_lossy().replace(' ', "\\ ")),
            Some(path)
        );
    }

    #[test]
    fn rejects_non_images_and_multiple_paths() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("not-an-image.png");
        std::fs::write(&path, b"not png").unwrap();

        assert_eq!(pasted_image_path(path.to_str().unwrap()), None);
        assert_eq!(pasted_image_path("/tmp/one.png /tmp/two.png"), None);
    }
}
