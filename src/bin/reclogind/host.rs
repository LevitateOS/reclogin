//! The machine's name and its language's clock style.

use std::path::Path;

pub const LOCALE_CONF: &str = "/etc/locale.conf";

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The system's language for times: `LC_TIME`, else `LANG`, from
/// `/etc/locale.conf` (both OSes write it there), else `C`.
pub fn time_locale(locale_conf: &Path) -> String {
    let text = std::fs::read_to_string(locale_conf).unwrap_or_default();
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let (k, v) = line.trim().split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
        })
    };
    value("LC_TIME")
        .or_else(|| value("LANG"))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "C".into())
}

/// Whether the clock shows 24 hours. The regions that write the time with
/// a.m. and p.m. use 12; everything else, `C` included, uses 24.
pub fn clock_24h(locale: &str) -> bool {
    let region = locale.split(['.', '@']).next().unwrap_or("");
    const TWELVE: &[&str] = &[
        "en_US", "en_CA", "en_AU", "en_NZ", "en_PH", "en_IN", "es_US", "hi_IN", "bn_BD", "ur_PK",
        "ar_EG", "ar_SA", "ko_KR",
    ];
    !TWELVE.contains(&region)
}
