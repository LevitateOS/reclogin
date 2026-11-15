# reclogin

One login screen for LevitateOS and AcornOS, in Rust. It replaces SDDM on
both systems with the same screen and behaviour; only the brand (name,
colours, mark, background) differs. It draws straight to the kernel's
display, with no X server, compositor or toolkit under it. The design is in
the LevitateOS repository: `docs/reclogin.md`, `docs/reclogin-technical.md`,
`docs/reclogin-plan.md` and the screens in `docs/reclogin-design/`.

Status: **stage 1 of the plan, written and compile-checked, never run** (see
"Not done").

## The two programs

| Program | Runs as | Does |
|---|---|---|
| `reclogind` | root | Takes console 7 and puts it in graphics mode, with console switching by signal. Opens the display device and is its master while console 7 is shown. Starts the greeter. Opens input devices when the greeter asks (only `/dev/input/event*` character devices, read-only, non-blocking) and revokes them when the console is left. Reads the users, sessions and keyboard layout. Gives everything back at exit (text mode, keyboard mode, the console it started from). |
| `reclogin-greeter` | the account `reclogin`, no rights | Receives the display and the input devices as open handles, sets the mode, draws the login screen on the processor (tiny-skia, cosmic-text with only the brand's fonts), reads input through libinput and libxkbcommon, and tells reclogind what the person chose and typed. |

They talk over a Unix `SOCK_SEQPACKET` socket pair, one JSON message per
packet; file handles travel beside the message that announces them
(`src/protocol.rs`, `src/wire.rs`). The greeter gets the socket as file
descriptor 3, an empty environment, no supplementary groups and
no-new-privileges. A typed password crosses the socket once, as an array of
byte values, and is overwritten on both sides after use.

## Files they read

| File | Read by | What |
|---|---|---|
| `/etc/reclogin.conf` | reclogind | `key = value` lines: `autologin`, `autologin_session`, `guest` (`on`/`off`, default `on`), `idle_seconds`. Unknown keys are reported and ignored. Stage 1 uses `guest` and `idle_seconds` only (passed to the screen). |
| `/etc/passwd`, `/etc/shells` | reclogind | The users in the list: uid 1000 to 60000, a shell from `/etc/shells` when it exists, an existing home. |
| `/usr/share/wayland-sessions/*.desktop`, `/usr/share/xsessions/*.desktop` | reclogind | The sessions. A Wayland and an X file of the same name: the Wayland one. |
| `/etc/X11/xorg.conf.d/*.conf` | reclogind | `XkbLayout`, `XkbVariant`, `XkbModel`, `XkbOptions`; `us` without them. Names from `/usr/share/X11/xkb/rules/evdev.xml`. |
| `/etc/locale.conf` | reclogind | `LC_TIME` or `LANG`, for a 12- or 24-hour clock. |
| `/usr/share/reclogin/` | greeter | The brand folder: `brand.json`, the background PNG it names, and the fonts in `fonts/` (or `../fonts/`). |
| `/etc/localtime` | greeter | The time zone of the clock. |
| `/dev/input/` (listing only), `/sys`, `/run/udev/data` | greeter | libinput's device list and properties. |

### Brand data in this repository

- `assets/levitateos/`, `assets/acornos/`: `brand.json` (name, the palette
  from each OS's `[look.dark]` in `distro/variants/<id>/distro.toml` plus
  `accent_ink`, `action` and `on_action`, the mark as SVG path data, the
  font families) and `background.png` (the website's frame art,
  `docs/website/public/art/levitate-frame.webp` and
  `docs/website/public/art/acorn/frame-hero.webp`, converted to PNG).
- `assets/fonts/`: Newsreader and Instrument Sans from the google/fonts
  repository (`ofl/newsreader`, `ofl/instrumentsans`), with their licences
  (`OFL-*.txt`, SIL Open Font License 1.1, no reserved font name). Only
  variable fonts exist there. `Newsreader.ttf`, `Newsreader-Italic.ttf` and
  `InstrumentSans.ttf` are the upstream files unchanged. `NewsreaderDisplay.ttf`
  and `NewsreaderDisplay-Italic.ttf` are derived from them: the optical-size
  axis fixed at 72 (fontTools `varLib.instancer`, weight axis kept) and the
  family renamed "Newsreader Display", because the text library sets the
  weight axis but not optical size, and the design's browser draws the clock
  and the greeting at large optical sizes.
- `assets/dev/state-*.json`: sample states for `--render-png`.

## Development switches

```sh
# One frame of the login screen to a PNG; opens no device and no socket.
reclogin-greeter --render-png out.png --brand-dir assets/levitateos \
    [--size 1280x800] [--state assets/dev/state-levitateos.json] \
    [--at '2026-11-15 21:42'] [--typed 12]

# The login screen on another console, with another greeter or account.
reclogind --console 8 --greeter target/debug/reclogin-greeter \
    --greeter-user nobody --config ./reclogin.conf --brand-dir assets/levitateos
```

## Not done

Everything after stage 1 of `docs/reclogin-plan.md`:

- logging in: PAM, sessions, X sessions (stages 2, 3). reclogind answers a
  login attempt with "Logging in is not built yet.";
- the session menu, the layout switcher, "Other user…", the no-users screen,
  remembering the last user and session (stage 4);
- who is logged in, returning to a session, power actions (stage 5; the
  system bus is used from there, at `/run/dbus/system_bus_socket`);
- automatic login (stage 6), the guest (stage 7);
- the on-screen keyboard, several monitors, idle switch-off, input devices
  plugged in after start (stage 8);
- restarting a crashed greeter and giving up after three crashes, the init
  services, the greeter's lock-down (stage 9);
- the per-OS build and shipping (stage 10).

**Nothing here was run on a console or a display.** It was compile-checked;
the only thing run was `--render-png`.
