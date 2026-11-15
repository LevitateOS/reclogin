//! The desktop sessions, from the session files the desktops install.
//!
//! `/usr/share/wayland-sessions/*.desktop` and `/usr/share/xsessions/*.desktop`.
//! A file is offered when it has a `Name=` and an `Exec=`, is not `Hidden`,
//! and its `TryExec=` (if any) exists. When a Wayland and an X file have the
//! same name, the Wayland one is offered. reclogin knows no desktop by name.

use std::collections::HashMap;
use std::path::Path;

use reclogin::protocol::{Session, SessionKind};

pub const WAYLAND_DIR: &str = "/usr/share/wayland-sessions";
pub const X11_DIR: &str = "/usr/share/xsessions";

pub fn installed(wayland_dir: &Path, x11_dir: &Path) -> Vec<Session> {
    let mut by_name: HashMap<String, Session> = HashMap::new();
    for (dir, kind) in [
        (x11_dir, SessionKind::X11),
        (wayland_dir, SessionKind::Wayland),
    ] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(session) = parse(&text, id, kind) else {
                continue;
            };
            // Wayland is read second, so it replaces an X file of the same name.
            by_name.insert(session.name.clone(), session);
        }
    }
    let mut sessions: Vec<Session> = by_name.into_values().collect();
    sessions.sort_by_key(|s| s.name.to_lowercase());
    sessions
}

fn parse(text: &str, id: String, kind: SessionKind) -> Option<Session> {
    let mut in_entry = false;
    let (mut name, mut exec, mut try_exec, mut hidden) = (None, None, None, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => name = Some(value.trim().to_string()),
            "Exec" => exec = Some(value.trim().to_string()),
            "TryExec" => try_exec = Some(value.trim().to_string()),
            "Hidden" => hidden = value.trim() == "true",
            _ => {}
        }
    }
    let (name, exec): (String, String) = (name?, exec?);
    if hidden || name.is_empty() || exec.is_empty() {
        return None;
    }
    if let Some(program) = try_exec {
        if !program_exists(&program) {
            return None;
        }
    }
    Some(Session { id, name, kind })
}

fn program_exists(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/local/sbin",
        "/usr/sbin",
        "/sbin",
    ]
    .iter()
    .any(|dir| Path::new(dir).join(program).is_file())
}
