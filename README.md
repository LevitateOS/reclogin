# reclogin

One login screen for LevitateOS and AcornOS, in Rust — replacing SDDM on both systems with the same greeter and behavior; only branding differs (from the distro profile).

Status: **scaffolding only.** The design — PAM authentication, logind/elogind seat/session handling so one implementation serves systemd and OpenRC systems, session files as data, console fallback if the greeter fails, and no agent integration of any kind — is recorded in `docs/reclogin.md` in the LevitateOS repository. Read it before writing code here.
