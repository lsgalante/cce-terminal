//! Where a window was, so a restart or a reopen brings it back there: its
//! tabs, each in the directory its shell was in, and which one was shown.
//! Saved when the window exits (`on_exit` — which the compositor's
//! `restart-app` runs as any close does), taken back when one launches.
//! Shells cannot survive; a restored tab is a fresh shell in that directory.
//! A window that ended because its last shell exited saves nothing.
//!
//! **Keyed by the launch's argv**, because that is what a restart replays:
//! the same command line comes back and finds its place, while one made
//! differently (a launcher's `Terminal=true` entry, `-e <cmd>`) is a new
//! request and starts as asked.
//!
//! **A command is never restored, only re-run by its own argv.** The tab
//! that ran the argv's command is saved as a marker, not as the command, so
//! nothing in the file can start a program. Under the same argv the launch
//! runs its command anyway — that is the command line, and refusing it would
//! turn a launcher's `htop` window into a bare shell after its first close —
//! so it takes the marked tab's place; the other tabs come back as shells.
//! A command window with nothing beyond its command is not kept at all.
//!
//! **Each key holds a short stack**, as cce-code-agent's `resume.rs` does:
//! every window that exits pushes its place and every launch takes the
//! newest, so a `restart-app` over several identical windows gives each one
//! a place back rather than all of them the same tabs.
//!
//! `cce_state_dir()/terminal/state.json`, `{"<key>": [place, ...]}`, reread
//! and merged under a `flock` on every change (each window is its own
//! process, and a restart closes them all at once) and renamed over.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Places kept per launch: more windows opened the same way than this, and
/// the oldest are forgotten.
const KEPT: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct SavedTab {
    /// The shell's directory (`None`: wherever the launch starts).
    pub cwd: Option<PathBuf>,
    /// This tab ran the argv's command rather than a shell.
    pub command: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Place {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
}

pub fn path() -> PathBuf {
    cce_ui::config::cce_state_dir().join("terminal").join("state.json")
}

/// This window's launch, and where places are kept.
pub struct Launch {
    key: String,
    path: PathBuf,
}

impl Launch {
    pub fn new(path: PathBuf, argv: &[String]) -> Launch {
        Launch { key: key(argv), path }
    }

    /// Take the newest place kept for this launch, if any.
    pub fn take(&self) -> Option<Place> {
        self.edit(|file| take(file, &self.key))
    }

    /// Keep `place` for the next launch made the same way.
    pub fn push(&self, place: &Place) {
        if place.tabs.iter().all(|t| t.command) {
            return; // the argv alone brings this back
        }
        self.edit(|file| push(file, &self.key, place));
    }

    /// Read, change and write the file under an exclusive lock.
    fn edit<T>(&self, f: impl FnOnce(&mut Map<String, Value>) -> T) -> T {
        let lock = self.path.parent().and_then(|dir| {
            std::fs::create_dir_all(dir).ok()?;
            File::options().create(true).truncate(false).write(true).open(dir.join("state.lock")).ok()
        });
        if let Some(lock) = &lock {
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        }
        let text = std::fs::read_to_string(&self.path).ok();
        let mut file = match text.and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let before = file.clone();
        let out = f(&mut file);
        if file == before {
            return out;
        }
        let tmp = self.path.with_extension(format!("json.{}", std::process::id()));
        let res = std::fs::write(&tmp, Value::Object(file).to_string())
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = res {
            log::warn!("cce-terminal: cannot save the window's tabs to {}: {e}", self.path.display());
            let _ = std::fs::remove_file(&tmp);
        }
        out // the lock goes with the file
    }
}

/// A launch's key: its argv, hashed (it can be long, and need not sit in a
/// file in the clear) with FNV-1a, which is stable across builds.
fn key(argv: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for arg in argv {
        for b in arg.bytes().chain([0xff]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

fn take(file: &mut Map<String, Value>, key: &str) -> Option<Place> {
    let stack = file.get_mut(key)?.as_array_mut()?;
    let place = stack.pop().map(|v| from_json(&v));
    if stack.is_empty() {
        file.remove(key);
    }
    place
}

fn push(file: &mut Map<String, Value>, key: &str, place: &Place) {
    let stack = file.entry(key.to_string()).or_insert_with(|| Value::Array(Vec::new()));
    if !stack.is_array() {
        *stack = Value::Array(Vec::new());
    }
    let list = stack.as_array_mut().expect("made an array above");
    list.push(to_json(place));
    let over = list.len().saturating_sub(KEPT);
    list.drain(..over);
}

/// The tabs to open: the saved place with its vanished directories dropped
/// to the launch's own, the argv's command (if any) in its marked tab or
/// first; nothing saved, one tab as the launch asks.
pub fn plan(command: bool, saved: Option<Place>, is_dir: impl Fn(&Path) -> bool) -> Place {
    let mut place = saved.filter(|p| !p.tabs.is_empty()).unwrap_or_default();
    for tab in &mut place.tabs {
        tab.cwd = tab.cwd.take().filter(|d| is_dir(d));
        // Only this launch's command line runs a command.
        tab.command &= command;
    }
    if command && !place.tabs.iter().any(|t| t.command) {
        place.tabs.insert(0, SavedTab { cwd: None, command: true });
        place.active += 1;
    }
    if place.tabs.is_empty() {
        place.tabs.push(SavedTab { cwd: None, command: false });
    }
    place.active = place.active.min(place.tabs.len() - 1);
    place
}

fn to_json(p: &Place) -> Value {
    let tabs: Vec<Value> = p
        .tabs
        .iter()
        .map(|t| json!({ "cwd": t.cwd.as_ref().map(|d| d.to_string_lossy()), "command": t.command }))
        .collect();
    json!({ "tabs": tabs, "active": p.active })
}

fn from_json(v: &Value) -> Place {
    let tabs = v.get("tabs").and_then(Value::as_array).map_or(Vec::new(), |tabs| {
        tabs.iter()
            .map(|t| SavedTab {
                cwd: t.get("cwd").and_then(Value::as_str).map(PathBuf::from),
                command: t.get("command").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect()
    });
    let active = v.get("active").and_then(Value::as_u64).unwrap_or(0) as usize;
    Place { tabs, active }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn shells(dirs: &[&str], active: usize) -> Place {
        let tabs = dirs.iter().map(|d| SavedTab { cwd: Some(d.into()), command: false }).collect();
        Place { tabs, active }
    }

    #[test]
    fn the_same_argv_takes_its_tabs_back() {
        let mut file = Map::new();
        let plain = key(&[]);
        push(&mut file, &plain, &shells(&["/a", "/b"], 1));
        // A launch naming a command is a new request.
        assert_eq!(take(&mut file, &key(&argv(&["-e", "htop"]))), None);
        assert_eq!(take(&mut file, &plain), Some(shells(&["/a", "/b"], 1)));
        assert_eq!(take(&mut file, &plain), None, "taken, not copied");
        assert!(file.is_empty());
        // The argv is the key, split where the arguments split.
        assert_ne!(key(&argv(&["ab", "c"])), key(&argv(&["a", "bc"])));
    }

    #[test]
    fn windows_launched_alike_each_get_their_own() {
        let mut file = Map::new();
        let k = key(&[]);
        push(&mut file, &k, &shells(&["/first"], 0));
        push(&mut file, &k, &shells(&["/second"], 0));
        assert_eq!(take(&mut file, &k), Some(shells(&["/second"], 0)));
        assert_eq!(take(&mut file, &k), Some(shells(&["/first"], 0)));
        for i in 0..20 {
            push(&mut file, &k, &shells(&[&format!("/{i}")], 0));
        }
        let mut n = 0;
        while take(&mut file, &k).is_some() {
            n += 1;
        }
        assert_eq!(n, KEPT, "the oldest are forgotten");
    }

    #[test]
    fn plan_restores_tabs_and_drops_vanished_directories() {
        let exists = |d: &Path| d != Path::new("/gone");
        // Nothing saved: one tab, as the launch asks.
        assert_eq!(plan(false, None, exists), Place { tabs: vec![SavedTab { cwd: None, command: false }], active: 0 });
        let p = plan(false, Some(shells(&["/a", "/gone", "/c"], 2)), exists);
        assert_eq!(p.tabs.iter().map(|t| t.cwd.clone()).collect::<Vec<_>>(), [Some("/a".into()), None, Some("/c".into())]);
        assert_eq!(p.active, 2);
        assert!(p.tabs.iter().all(|t| !t.command));
    }

    #[test]
    fn plan_runs_only_the_launchs_command() {
        let any = |_: &Path| true;
        // The command's tab keeps its place; the rest are shells.
        let mut saved = shells(&["/a", "/b"], 1);
        saved.tabs[1].command = true;
        let p = plan(true, Some(saved.clone()), any);
        assert_eq!(p.tabs.iter().map(|t| t.command).collect::<Vec<_>>(), [false, true]);
        // A plain launch never runs one, whatever the file says.
        assert!(plan(false, Some(saved), any).tabs.iter().all(|t| !t.command));
        // The command's tab had closed: the command still runs, first.
        let p = plan(true, Some(shells(&["/a"], 0)), any);
        assert_eq!(p.tabs.iter().map(|t| t.command).collect::<Vec<_>>(), [true, false]);
        assert_eq!(p.active, 1);
    }

    #[test]
    fn a_command_window_with_nothing_more_is_not_kept() {
        let path = std::env::temp_dir().join(format!("cce-terminal-resume-{}", std::process::id())).join("state.json");
        let launch = Launch::new(path.clone(), &argv(&["htop"]));
        launch.push(&Place { tabs: vec![SavedTab { cwd: Some("/a".into()), command: true }], active: 0 });
        launch.push(&Place::default());
        assert_eq!(launch.take(), None);
        let place = Place { tabs: vec![SavedTab { cwd: None, command: true }, SavedTab { cwd: Some("/b".into()), command: false }], active: 1 };
        launch.push(&place);
        assert_eq!(launch.take(), Some(place), "round-trips through the file");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
