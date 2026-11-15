//! reclogin — one login screen for LevitateOS and AcornOS, in Rust.
//!
//! The design is recorded in `docs/reclogin.md` in the LevitateOS repo:
//! the same greeter and behavior on both systems (PAM for auth,
//! logind/elogind for seats/sessions, distro profile only for branding),
//! replacing SDDM on both. No agent integration of any kind.
//!
//! Status: scaffolding only. Nothing here authenticates anyone yet.

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version") | Some("-V") => {
            println!("reclogin {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") | None => {
            println!("reclogin — login manager (not implemented yet)");
            println!("usage: reclogin [--version]");
        }
        Some(other) => {
            eprintln!("reclogin: unknown argument {other:?} (try --help)");
            std::process::exit(2);
        }
    }
}
