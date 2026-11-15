# Switching both OSes from SDDM to reclogin

`login-screen.rhai` beside this file is the `login-screen` group recipe for
both OSes, prepared and not active. SDDM stays the login screen until this
switch is made. Make it only after a `login-screen` prebuilt has been built and
published for **both** distros. Line numbers are as of 2026-11-15; when they
have moved, look for the quoted text.

## 0. Build and publish the prebuilts (the owner starts these)

```sh
cargo xtask base lock levitate      # once, after the apps_build_packages change
cargo xtask base lock acorn         #   (Rust, pam/libinput/libxkbcommon -dev)
cargo xtask prebuilt build login-screen levitate   # compiles in the apps-build root; needs root
cargo xtask prebuilt build login-screen acorn
cargo xtask prebuilt publish login-screen levitate
cargo xtask prebuilt publish login-screen acorn
```

Each publish prints the release `login-screen-<key16>` in LevitateOS/prebuilt.
Its asset is `login-screen-levitate.tar.gz` / `login-screen-acorn.tar.gz`.

## 1. The recipe

1. Copy `tools/reclogin/dist/recipe/login-screen.rhai` over
   `recipes/login-screen.rhai` (replace the whole file: the SDDM group, its
   `SEEDS_MUSL = ["sddm"]`, its `ACCOUNTS_MUSL` sddm lines, both generated pin
   blocks and the Maldives theme configuration all go).
2. In the copy, fill `const PINS = #{};` with the two releases, as
   `recipes/rec-apps.rhai` has them:
   ```
   const PINS = #{
       glibc: ["login-screen-levitate.tar.gz",
               "https://github.com/LevitateOS/prebuilt/releases/download/login-screen-<key16>/login-screen-levitate.tar.gz",
               "<sha256 GitHub shows for the asset>"],
       musl:  ["login-screen-acorn.tar.gz",
               "https://github.com/LevitateOS/prebuilt/releases/download/login-screen-<key16>/login-screen-acorn.tar.gz",
               "<sha256>"],
   };
   ```
   and set `network_max_bytes` in `NEEDS` to the larger archive's size,
   rounded up.
3. In the header's "NOT ACTIVE YET" paragraph (lines 6-10 of the copy),
   delete everything before "It names no packages", which stays true.
4. Check it: `target/release/recipe info recipes/login-screen.rhai`.

## 2. SDDM out of `recipes/plasma.rhai`

- Line 43, in `SEEDS_GLIBC`: remove `"sddm", "sddm-breeze", "sddm-wayland-plasma"`
  (keep the line's other seeds; the list then ends with `"udisks2"`).
- Lines 763-764, the comment above `ctx.os = RPM_OS;`: "boot to SDDM
  (display-manager.service) with KDE's Breeze theme and the OS's wallpaper
  behind it," goes.
- Lines 779-783, in `configure`: remove the comment "Boot into SDDM, which
  takes the first virtual console from its getty.", `let sddm = ...`,
  `tree_text(ctx, sddm);`, the `display-manager.service` link and the
  `getty@tty1.service` mask (`put_link(ctx, "etc/systemd/system/getty@tty1.service", "/dev/null");`).
  The login screen is on console 7 now; console 1 keeps its text login.
- Lines 788-796: the whole "The login screen: KDE's Breeze theme ..." block
  (`let themes = "usr/share/sddm/themes/";` to the `etc/sddm.conf.d/10-theme.conf` line).
- Then `fn wallpaper_path` (line 773) has no caller left: remove it too, if
  nothing else calls it by then.
- The generated pin block loses `sddm`, `sddm-breeze`, `sddm-wayland-plasma`
  and `kde-settings-sddm` when step 4 regenerates it; do not edit it by hand.

## 3. The default sets (`distro/variants/*/distro.toml`, `[desktop] recipes`)

- LevitateOS (`distro/variants/levitate/distro.toml`, lines 205-211): add
  `"login-screen"` after `"plasma"` (SDDM came with `plasma` until now).
- AcornOS (`distro/variants/acorn/distro.toml`, line 177): already names
  `"login-screen"`; nothing changes.

## 4. Locks and pins

```sh
cargo xtask base lock levitate
cargo xtask base lock acorn
cargo xtask base groups levitate
cargo xtask base groups acorn
cargo xtask base groups levitate --check
cargo xtask base groups acorn --check
```

`base lock` drops what only the SDDM seeds needed from the desktop locks
(`sddm*`, `kde-settings-sddm`, on AcornOS `sddm-openrc`, `sddm-greeter-qt6`,
and `upower`/`upower-udev` unless another group needs them); `base groups`
rewrites the pin blocks of `plasma.rhai` and of every group that took over a
package. The new `login-screen.rhai` has no seeds and no pin blocks, so the
generators leave it alone.

## 5. The installer's automatic login

- `distro/builder/guest/installed/system-setup.sh`, lines 77-83, the
  `autologin)` case: write reclogin's settings instead of the SDDM drop-in.
  The group has installed `/etc/reclogin.conf` (the software step runs before
  this one), so replace its `autologin` and `autologin_session` lines and keep
  the rest:
  ```sh
  autologin)
      # reclogin logs user $2 in to session $3 (a session file's name
      # without .desktop) at every boot without showing the login screen.
      conf=/etc/reclogin.conf
      tmp=$(mktemp /etc/reclogin.conf.XXXXXX)
      if [ -f "$conf" ]; then
          awk '!/^[[:space:]]*autologin(_session)?[[:space:]]*=/' "$conf" > "$tmp"
      fi
      printf 'autologin = %s\nautologin_session = %s\n' "$2" "$3" >> "$tmp"
      chmod 0644 "$tmp"
      mv "$tmp" "$conf"
      ;;
  ```
  The session recsetup passes (`Desktop::session()`: `plasma`, `lxqt`) is the
  name of `/usr/share/wayland-sessions/plasma.desktop` and
  `/usr/share/xsessions/lxqt.desktop` without the ending, which is what
  `autologin_session` takes. Recipe keeps an `/etc` file the administrator
  changed (it writes `NAME.recipe-new` beside it on an update), so editing the
  group's file is safe.
- `tools/recstrap/src/bin/recsetup/plan.rs`, lines 390-396: the check
  `root.has_program("sddm")` becomes `root.has_program("reclogind")`, and its
  message "the target has no SDDM, whose autologin this sets" becomes "the
  target has no login screen (reclogin), whose automatic login this sets".
  The comment above it ("The default set brings the display manager ...")
  stays true.
- reclogin's own automatic login is stage 6 of `docs/reclogin-plan.md`; check
  it is written before the switch, or the installer's line has no effect.

## 6. Texts that become false

- `distro/builder/os-recipes/xlibre.rhai` line 4 and line 251, and
  `recipes/xlibre.rhai` line 362: "SDDM's /usr/bin/X", "the link SDDM starts":
  reclogin starts `/usr/bin/X` now (`tools/reclogin/src/bin/reclogind/xserver.rs`).
- `distro/builder/src/artifact/rootfs.rs` lines 147-150: the SDDM example for
  non-epoch mtimes (keep the rule, drop or reword the example).
- `docs/reclogin-technical.md` section 9 and `docs/reclogin-plan.md` stage 10:
  mark the switch done; `tools/reclogin/README.md` "Not done": the shipping.
- `distro/builder/src/login_screen.rs` module docs and `prebuilt.rs`: "not yet
  in `recipes/`" goes.
- Public wording about the login screen (website, `docs/desktop.md`): read
  `docs/product-handoff.md` first, as `AGENTS.md` requires.

## 7. Check without building

- `target/release/recipe info recipes/login-screen.rhai` and
  `... recipes/plasma.rhai`.
- `cargo check --workspace` (after the recsetup change).
- `cargo xtask base groups levitate --check` and `... acorn --check`.

At the owner's next desktop build, `cargo xtask test login <distro> --flavour desktop`
(after an install from the release ISO) checks the result: reclogind and the
greeter run, the greeter as `reclogin`, console 7 shown, no SDDM left, a text
login on console 1.
