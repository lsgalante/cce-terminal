//! PTY plumbing: openpty, shell spawn with the slave as controlling terminal,
//! and dup'd master handles for the reader thread / key-input writes.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    pub master: OwnedFd,
    pub child: Child,
}

/// Open a pty pair and spawn `command` (or `$SHELL`) on the slave side, in
/// its own session with the slave as controlling terminal. The slave fd is
/// fully handed to the child (stdin/stdout/stderr) and closed in the parent,
/// so EOF on the master is the child-exit signal. `cwd` starts the child
/// there (a new tab inherits the active one's directory); `None` inherits
/// the terminal's own.
pub fn spawn_shell(
    cols: u16,
    rows: u16,
    command: Option<&[String]>,
    cwd: Option<&std::path::Path>,
) -> io::Result<Pty> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let ret = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws)
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

    let mut cmd = match command {
        Some(argv) => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
        None => Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())),
    };
    // The VT layer is alacritty_terminal, so alacritty's terminfo entry
    // describes us accurately (verified present on the host).
    cmd.env("TERM", "alacritty")
        .env("COLORTERM", "truecolor")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // A directory that vanished since it was read is not worth failing the
    // spawn over — the child just starts where the terminal did.
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // stdin IS the slave after the Stdio wiring above.
            if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(Pty { master, child })
}

impl Pty {
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }

    /// A dup of the master as a `File` (own fd, CLOEXEC) — one for the reader
    /// thread, one for key-input writes.
    pub fn dup_handle(&self) -> io::Result<File> {
        let fd = unsafe { libc::fcntl(self.master.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    /// Full round trip through a real shell: keystroke bytes in on the master,
    /// command output back out — the headless equivalent of typing into the
    /// window (the GUI path is `handle_key_input` → the same master fd).
    ///
    /// The child's whole environment is pinned through `env -i` rather than
    /// inherited, and this is load-bearing twice over.
    ///
    /// It used to spawn `$SHELL` with the developer's environment, type a
    /// command and let the shell `exit` — which is exactly when an
    /// interactive shell flushes its history file. So `cargo test` appended
    /// `printf 'RT-%s\n' OK; exit` to the developer's own shell history,
    /// intermittently: it is a race with the `kill` below, and reproduced in
    /// 2 runs out of 8. Appended rather than truncated, so nothing was lost,
    /// but a test has no business writing there at all.
    ///
    /// A private `HOME` alone does NOT fix it. `HISTFILE` is commonly
    /// exported from an rc file as an ABSOLUTE path — `export
    /// HISTFILE="$HOME/.cache/.zsh_history"` is the shape — so the test
    /// binary inherits it and the child writes through to the real one no
    /// matter where its home points. Clearing the environment is what closes
    /// that, and `HISTFILE=/dev/null` then covers the shells that would
    /// otherwise default back into the private home.
    ///
    /// Second, `$SHELL` made the test mean something different on every
    /// machine: it sourced the developer's rc files, so a slow, noisy or
    /// interactive one could hang this or break its assertion, and a
    /// non-POSIX login shell would not read the command at all. What is
    /// under test is the pty plumbing — openpty, the controlling terminal,
    /// the master round trip — not which binary the developer logs in with.
    /// The cost is that `spawn_shell`'s `$SHELL` fallback is no longer
    /// covered here; it is one `env::var` with a `/bin/sh` default.
    #[test]
    fn shell_round_trip() {
        let home = std::env::temp_dir()
            .join(format!("cce-terminal-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("test home");
        let argv: Vec<String> = [
            "/usr/bin/env".to_string(),
            "-i".to_string(),
            "PATH=/usr/bin:/bin".to_string(),
            format!("HOME={}", home.display()),
            "HISTFILE=/dev/null".to_string(),
            // spawn_shell sets these on the Command, and `env -i` would
            // otherwise drop them before the shell ever sees them.
            "TERM=alacritty".to_string(),
            "COLORTERM=truecolor".to_string(),
            "/bin/sh".to_string(),
        ]
        .to_vec();
        let mut pty = spawn_shell(80, 24, Some(&argv), None).expect("openpty/spawn");
        let mut writer = pty.dup_handle().unwrap();
        let mut reader = pty.dup_handle().unwrap();
        writer.write_all(b"printf 'RT-%s\\n' OK; exit\r").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF/EIO: shell exited
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&out).contains("RT-OK") {
                        break;
                    }
                }
            }
        }
        assert!(
            String::from_utf8_lossy(&out).contains("RT-OK"),
            "no round-trip output; got: {:?}",
            String::from_utf8_lossy(&out)
        );
        let _ = pty.child.kill();
        let _ = pty.child.wait();
        let _ = std::fs::remove_dir_all(&home);
    }
}
