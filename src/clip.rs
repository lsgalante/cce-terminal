//! PRIMARY-selection clipboard, via the same wl-copy/wl-paste convention as
//! the toolkit's regular clipboard (`cce_ui::widget::clipboard`): select →
//! primary, middle-click → paste primary. No X fallback — this DE is
//! Wayland-only and the toolkit's xclip arm is vestigial.

use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy_primary(text: &str) {
    let text = text.to_string();
    std::thread::spawn(move || {
        if let Ok(mut child) =
            Command::new("wl-copy").arg("--primary").stdin(Stdio::piped()).spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    });
}

pub fn paste_primary() -> Option<String> {
    let output = Command::new("wl-paste").args(["--primary", "-n"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
