//! The desktop sessions, from the session files the desktops install.
//!
//! `/usr/share/wayland-sessions/*.desktop` and `/usr/share/xsessions/*.desktop`.
//! A file is offered when it has a `Name=` and an `Exec=`, is neither
//! `Hidden=true` nor `NoDisplay=true`, and the program it starts exists: its
//! `TryExec=` when it has one, and always the program `Exec=` names. When a
//! Wayland and an X file have the same name (the same file name, or the same
//! `Name=`), the Wayland one is offered. reclogin knows no desktop by name.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use reclogin::protocol::{Session, SessionKind};

pub const WAYLAND_DIR: &str = "/usr/share/wayland-sessions";
pub const X11_DIR: &str = "/usr/share/xsessions";

pub fn installed(wayland_dir: &Path, x11_dir: &Path) -> Vec<Session> {
    // By file name first: `Session::id` must name exactly one session.
    let mut by_id: HashMap<String, Session> = HashMap::new();
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
            by_id.insert(session.id.clone(), session);
        }
    }
    // Then by the name shown: two entries that read the same to the person
    // are one, and a Wayland one beats an X one.
    let mut all: Vec<Session> = by_id.into_values().collect();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    let mut by_name: HashMap<String, Session> = HashMap::new();
    for session in all {
        match by_name.get(&session.name) {
            Some(kept)
                if !(kept.kind == SessionKind::X11 && session.kind == SessionKind::Wayland) => {}
            _ => {
                by_name.insert(session.name.clone(), session);
            }
        }
    }
    let mut sessions: Vec<Session> = by_name.into_values().collect();
    sessions.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    sessions
}

fn parse(text: &str, id: String, kind: SessionKind) -> Option<Session> {
    let mut in_entry = false;
    let (mut name, mut exec, mut try_exec) = (None, None, None);
    let mut hidden = false;
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
            "Hidden" | "NoDisplay" => hidden |= value.trim() == "true",
            _ => {}
        }
    }
    let (name, exec): (String, String) = (name?, exec?);
    if hidden || name.is_empty() || exec.is_empty() {
        return None;
    }
    if let Some(program) = try_exec.filter(|p| !p.is_empty()) {
        if !program_exists(&program) {
            return None;
        }
    }
    if !program_exists(&exec_program(&exec)?) {
        return None;
    }
    Some(Session { id, name, kind })
}

/// The program an `Exec=` line starts: its first word (a double-quoted one
/// may hold spaces), after `env` and its `NAME=value` words when it starts
/// with them.
fn exec_program(exec: &str) -> Option<String> {
    let mut words = Vec::new();
    let mut rest = exec.trim_start();
    while !rest.is_empty() && words.len() < 16 {
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"')?;
            words.push(quoted[..end].to_string());
            rest = quoted[end + 1..].trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            words.push(rest[..end].to_string());
            rest = rest[end..].trim_start();
        }
    }
    let mut words = words.into_iter();
    let first = words.next()?;
    if first != "env" && !first.ends_with("/env") {
        return Some(first);
    }
    words.find(|w| !w.starts_with('-') && !w.contains('='))
}

fn program_exists(program: &str) -> bool {
    let runnable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return runnable(Path::new(program));
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
    .any(|dir| runnable(&Path::new(dir).join(program)))
}
