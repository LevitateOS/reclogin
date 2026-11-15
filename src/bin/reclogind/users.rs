//! The users that may be chosen from the list.
//!
//! From `/etc/passwd`: uid 1000 to 60000, a login shell listed in
//! `/etc/shells` (when that file exists), and a home folder that exists.
//! Root and system accounts are never listed. AccountsService is not read.

use std::collections::HashSet;
use std::path::Path;

use reclogin::protocol::User;

pub fn listable(passwd: &Path, shells: &Path) -> Vec<User> {
    let Ok(text) = std::fs::read_to_string(passwd) else {
        return Vec::new();
    };
    let shells: Option<HashSet<String>> = std::fs::read_to_string(shells).ok().map(|text| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    });
    let mut users = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 || fields[0].is_empty() || fields[0].starts_with(['+', '-', '#']) {
            continue;
        }
        let Ok(uid) = fields[2].parse::<u32>() else {
            continue;
        };
        if !(1000..=60000).contains(&uid) {
            continue;
        }
        let shell = fields[6];
        if let Some(shells) = &shells {
            if !shells.contains(shell) {
                continue;
            }
        } else if shell.ends_with("/nologin") || shell.ends_with("/false") || shell.is_empty() {
            continue;
        }
        if fields[5].is_empty() || !Path::new(fields[5]).is_dir() {
            continue;
        }
        let gecos = fields[4].split(',').next().unwrap_or("").trim();
        let real_name = if gecos.is_empty() { fields[0] } else { gecos };
        users.push(User {
            name: fields[0].to_string(),
            real_name: real_name.to_string(),
            logged_in: false,
        });
    }
    users
}
