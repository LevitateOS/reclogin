# reclogin

One login screen for LevitateOS and AcornOS, in Rust. It replaces SDDM on
both systems with the same screen and behaviour; only the brand (name,
colours, mark, background) differs. It draws straight to the kernel's
display, with no X server, compositor or toolkit under it. The design is in
the LevitateOS repository: `docs/reclogin.md`, `docs/reclogin-technical.md`,
`docs/reclogin-plan.md` and the screens in `docs/reclogin-design/`.

Status: **stages 1, 2, 3, 4, 5, 7 and 9 of the plan, written and compile-checked,
never run** (see "Not done").

## The two programs

| Program | Runs as | Does |
|---|---|---|
| `reclogind` | root | Takes console 7 and puts it in graphics mode, with console switching by signal. Opens the display device and is its master while console 7 is shown. Starts the greeter. Opens input devices when the greeter asks (only `/dev/input/event*` character devices, read-only, non-blocking) and revokes them when the console is left. Reads the users, sessions and keyboard layout. Gives everything back at exit (text mode, keyboard mode, the console it started from). |
| `reclogind --login-service` | root, started by `reclogind` | The login-service helper (stage 5, below): the only code that talks to logind or elogind over the system bus. Hidden mode, like `--session-worker`. |
| `reclogin-greeter` | the account `reclogin`, no rights | Receives the display and the input devices as open handles, sets the mode, draws the login screen on the processor (tiny-skia, cosmic-text with only the brand's fonts), reads input through libinput and libxkbcommon, and tells reclogind what the person chose and typed. |

They talk over a Unix `SOCK_SEQPACKET` socket pair, one JSON message per
packet; file handles travel beside the message that announces them
(`src/protocol.rs`, `src/wire.rs`). The greeter gets the socket as file
descriptor 3, an empty environment, no supplementary groups and
no-new-privileges. A typed password never goes into JSON: `answer` announces
it and it crosses as the next packet, its raw bytes, read straight into
storage that is overwritten after use (see "Secrets on the way").

## Files they read

| File | Read by | What |
|---|---|---|
| `/etc/reclogin.conf` | reclogind | `key = value` lines: `autologin`, `autologin_session`, `guest` (`on`/`off`, default `on`), `idle_seconds`. Unknown keys are reported and ignored. `autologin` and `autologin_session`: see "Automatic login". |
| `/etc/passwd`, `/etc/shells` | reclogind | The users in the list: uid 1000 to 60000, a shell from `/etc/shells` when it exists, an existing home. |
| `/usr/share/wayland-sessions/*.desktop`, `/usr/share/xsessions/*.desktop` | reclogind | The sessions. Not offered: `Hidden=true` or `NoDisplay=true`, a `TryExec` or an `Exec` program that does not exist. A Wayland and an X file of the same name (file name or `Name=`): the Wayland one. |
| `/etc/X11/xorg.conf.d/*.conf` | reclogind | `XkbLayout`, `XkbVariant`, `XkbModel`, `XkbOptions` (both OSes: the installer's `x11-keyboard` writes `00-keyboard.conf`); a comma list is several layouts; `us` without them. Names from `/usr/share/X11/xkb/rules/evdev.xml`. |
| `/etc/vconsole.conf` | reclogind | `XKBLAYOUT`, `XKBVARIANT`, `XKBMODEL`, `XKBOPTIONS`, only when `xorg.conf.d` gave no layout (systemd's place, so LevitateOS). `KEYMAP=` (and OpenRC's `/etc/conf.d/loadkeys`) is a console keymap, not an XKB layout, and is not read. |
| `/var/lib/reclogin/last.json` | reclogind (reads and writes) | The last user and each user's last session. Root's: folder 0700, file 0600, replaced atomically; missing or damaged is the same as empty. |
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
- `assets/dev/state-*.json`: sample states for `--render-png`. A file may
  also carry a `"sample"` object that puts the form where PAM would leave it:
  `state-levitateos-wrong-password.json` (a failed attempt with Caps Lock on)
  and `state-acornos-expired-password.json` (an expired password being
  changed). The screens of stage 4 also read `row` (`guest`, `other` or a user
  name), `user_name`, `focus` and `menu` there. Draw them with `--at '2026-11-15 21:43'` and
  `--at '2026-11-16 08:15'` to match the design files.

## Development switches

```sh
# One frame of the login screen to a PNG; opens no device and no socket.
reclogin-greeter --render-png out.png --brand-dir assets/levitateos \
    [--size 1280x800] [--state assets/dev/state-levitateos.json] \
    [--at '2026-11-15 21:42'] [--typed 12] [--second-screen]

# The login screen on another console, with another greeter or account.
reclogind --console 8 --greeter target/debug/reclogin-greeter \
    --greeter-user nobody --config ./reclogin.conf --brand-dir assets/levitateos

# The same without the greeter's lock-down (below); reclogind passes
# --no-sandbox on to the greeter, which otherwise refuses to run unlocked.
reclogind --no-sandbox ...
```

`--render-png` is not affected by any of the lock-down. On a development
machine without libinput's development files, link the greeter against the
library itself through a folder holding a `libinput.so` link to
`/usr/lib64/libinput.so.10`:
`RUSTFLAGS="-L <that folder>" cargo build -p reclogin --bin reclogin-greeter`.

## Logging in (stage 2)

One login at a time. For each one reclogind starts itself again as a
**session worker** (`reclogind --session-worker`, `src/bin/reclogind/worker.rs`):
a separate single-threaded root process with its end of a private socket
pair as descriptor 3, the way `sddm-helper` and `gdm-session-worker` work.
The worker owns the login's PAM handle from start to end. PAM's
conversation is synchronous, so it must not block reclogind's main loop; and
the login service registers the process that opens the PAM session as the
session's first process, which must not be reclogind.

Worker and reclogind speak their own small message set (same socket kind and
framing as the greeter's): to the worker `start {user, session, console}`,
`answer` (the secret follows as a raw packet), `cancel`; from the worker
`prompt {kind, text}`, `message {kind, text}`, `failed {text}`, `accepted`,
`ready_to_start {console, x_server, x_pidfd}`, `started {console, desktop,
pidfd}` (the desktop's process group; with `x_pidfd`/`pidfd` true the
process's descriptor from `pidfd_open` comes with the message as a handle),
`ended {status}`, and `hold {on}` around the steps that must not be cut
short (the guest's cleanup and home, opening and closing the PAM session).
`start_guest {session, console, wait}` and `start_auto` take the place of
`start` for the guest and the automatic login.

### The PAM service

`reclogin` (`dist/pam/reclogin.levitateos` and `dist/pam/reclogin.acornos`,
installed as `/etc/pam.d/reclogin`). Each accepts what the OS's text-login
PAM service accepts: on LevitateOS the lines of util-linux's `login` over
systemd's `system-auth`, plus the optional wallet modules SDDM's file had;
on AcornOS linux-pam's own `login` (the `base-*` files, whose `base-session`
is expected to carry `pam_elogind`), plus an explicit
`-session optional pam_elogind.so` after it, which does nothing when
`base-session` already registered the session. The libpam functions are declared by hand
(`src/bin/reclogind/pam.rs`) so the crate compiles without libpam's
development files.

### What a login does, step by step

1. The greeter sends `choose_session` (if a session is chosen) and
   `begin {who}`, and keeps the typed password. reclogind checks the person
   (a listed user must be in the list; a typed name may be any account,
   root included, except `guest`; the guest is stage 7) and the session (installed; else
   the first one), chooses the session's console (the first one after the
   login console that nobody has open; consoles 1 to 6 keep their text
   logins), starts a worker and sends it `start`. While the last login's
   worker has not exited, and for one second after a failed or cancelled
   attempt, `begin` is answered with the message "Try again in a moment."
   and `auth_result {ok: false}` instead. Every `begin` gets exactly one
   `auth_result`.
2. The worker reads the session file. An X session is refused here with
   "No X server is installed." when there is neither `/usr/bin/X` nor
   `Xorg` on the path (see "X sessions" below).
3. `pam_start("reclogin", user)`, `PAM_TTY` set to `ttyN` of the chosen
   console (`PAM_RUSER` and `PAM_RHOST` unset), then `pam_authenticate`. Every question
   and message of PAM's conversation goes to the greeter in order and word
   for word (`prompt` / `message`); PAM's first question, when secret, is
   answered with the kept password, every other one is shown as a field.
   A failure is reported as "Authentication failed." unless PAM said why in
   an error message of its own; the log gets PAM's own description, the
   screen never does.
4. `pam_acct_mgmt`. On `PAM_NEW_AUTHTOK_REQD` the worker says "Current
   password accepted." (its own line, `MessageKind::Success`), passes on
   what PAM said meanwhile, and runs `pam_chauthtok(PAM_CHANGE_EXPIRED_AUTHTOK)`
   with its questions on the same screen ("Choose a new password.").
5. The worker says `accepted`. The console is handed to the user only now,
   after the account check.
6. The worker hands that console's device to the user (owner the user,
   group `tty`, mode 0620), puts `XDG_SEAT=seat0`,
   `XDG_VTNR`, `XDG_SESSION_TYPE`, `XDG_SESSION_CLASS=user` and
   `XDG_SESSION_DESKTOP` (the session file's first `DesktopNames`, else its
   name) into the PAM environment, then `initgroups` (the account's groups,
   so that `pam_group` can add to them), `pam_setcred(ESTABLISH)`,
   `pam_open_session`, `pam_setcred(REINITIALIZE)`, and says `ready_to_start`.
7. reclogind tells the greeter it succeeded, `pause`, gives the display and
   the input devices up, stops the greeter (an expected exit, marked so
   before anything is sent) and shows the session's console. It checks for
   five seconds that the console really is shown; if not, it tells the
   worker to cancel and brings the greeter back with "The desktop's screen
   could not be shown." While the session runs no greeter runs; if console 7
   is shown meanwhile, the session's console is shown again.
8. Once the console is shown (or after ten seconds) the worker forks the
   desktop's process: a new session with the console as its controlling
   terminal and standard input, the worker's groups as they are after
   `pam_setcred` (as login(1)), the user's gid and uid, PAM's environment plus
   `HOME`, `USER`, `LOGNAME`, `SHELL`, `PATH` (when PAM has none) and
   `XDG_CURRENT_DESKTOP` from `DesktopNames`. Its output and errors go to
   the user's own file, `$XDG_STATE_HOME/reclogin/session.log`, else
   `~/.local/state/reclogin/session.log`, opened as the user after the
   privilege drop, emptied at each login, mode 0600 (`/dev/null` when that
   fails), never into reclogind's log. `Exec` is split into words by the
   Desktop Entry rules (value escapes, double quotes, field codes dropped,
   `%%` kept as `%`), each word is single-quoted for the shell, and the
   result runs as `$SHELL -l -c "exec 'word' 'word'"` so the user's profile
   is read (`/bin/sh` instead when the shell is csh or tcsh, or is not an
   existing program); under
   `dbus-run-session --` when the login service gives no session bus (no
   `DBUS_SESSION_BUS_ADDRESS` in PAM's environment and no
   `/run/user/<uid>/bus`, waited for up to three seconds on a system started
   by systemd).
9. When the desktop's process exits (or reclogind asks the worker to end it:
   SIGTERM to its process group, SIGKILL after ten seconds), the worker
   hangs up the console, `pam_close_session`, `pam_setcred(DELETE)`, gives the
   console back to root (group `tty`, mode 0620; never a remembered owner),
   `pam_end`, says `ended` and exits.
10. When the worker has exited, reclogind starts a fresh greeter through
    the supervisor and shows console 7. The last user and their session
    (`last.rs`) were written at `started`.

`cancel` from the greeter, and a greeter replaced in the middle of a
conversation, ask the worker to end: it refuses PAM's conversation, closes
what it opened (PAM session, credentials, console) and exits by itself.
The worker is never killed to make room for a new login: reclogind waits
for it with its main loop running, and only after it is gone (and one
second has passed since a failed or cancelled attempt, so PAM's failure
delay cannot be skipped by cancelling) does a new `begin` start a worker.
A `cancel` that ended a login is answered with `auth_result {ok: false}` once
the worker is gone; a worker that ends before the greeter was let go
without saying `failed` (a signal while the X server started, say) gets
"The login could not be completed." then, so every attempt gets exactly one.
A worker that has not exited fifteen seconds after being asked (or a minute
after its desktop ended) is logged loudly and killed, except while it is in
a step it announced with `hold`, which holds the kill off for up to two
minutes (fifteen seconds again after the step), and reclogind gives its console back
itself (text mode, hang-up, root, `tty`, 0620). The greeter sends `cancel` at most
once per attempt, then shows "Cancelling…" and stays busy, sending no
`begin`, until that `auth_result` (or the attempt's own, if it crossed the
cancel) arrives; questions and messages that arrive meanwhile are dropped.

A worker that goes away while its desktop starts or runs (it crashed, was
killed) leaves the desktop to reclogind: it ends the desktop's and the X
server's process groups (reported in `ready_to_start` and `started`;
SIGTERM, SIGKILL after ten seconds, without stopping its loop; a group is
signalled only while its leader lives, checked through the process
descriptor sent with the message, since its number alone could belong to
another process by then; a group whose leader is gone is left to the login
service, and what stayed in it may run on until it ends by itself), gives the
console back, logs it, and only then shows the login screen. A panic in the
PAM conversation is caught and refuses the conversation.

The reverse: once its desktop runs, the worker does not need reclogind. It
stops asking for a signal when reclogind dies and ignores the loss of its
socket, keeps waiting for the desktop and closes the session by itself when
the desktop ends. So stopping, restarting or losing reclogind leaves running
desktops alone; before the desktop runs, losing reclogind ends the login.
A restarted reclogind does not own such a session, but it sees it: stage 5
lists the sessions through the login service, marks the user as logged in,
returns to it, and does not take its console from it. When reclogind is stopped it ends a login that is not yet running
the same way as a `cancel` and waits for the worker (at most fifteen
seconds).

### Secrets on the way

- Greeter: the typed text is in storage reserved up front and overwritten
  when cleared; it is copied into a `Secret` (overwritten when dropped). A
  secret answer leaves only its length on the screen.
- Both hops (greeter to reclogind, reclogind to the worker): the secret is
  never inside JSON. `answer` announces it and the next packet is its raw
  bytes, one marker byte first (`Channel::send_secret`), sent straight from
  the `Secret` without a copy and received straight into a buffer that is
  overwritten when dropped (`Channel::recv_secret`). (serde reads an
  internally tagged message into an intermediate buffer before it looks at
  its fields and frees it without overwriting; that is why no message
  carries a secret.) Message buffers are overwritten after use too, and a
  message that cannot be read is logged by its length only, never with
  serde's text, which can quote what it read.
- reclogind: the `Secret` goes to the worker as soon as it arrives and is
  dropped (overwritten); an answer to no open question is dropped the same
  way.
- Worker: the `Secret` is copied into a `malloc` buffer for PAM and
  overwritten as it goes out of scope. Replies not handed to PAM are
  overwritten before `free`.
- **Not overwritten by reclogin:** the buffer once PAM has it (Linux-PAM's
  modules overwrite it before `free`, reclogin cannot check), the copies PAM
  and its modules keep (`PAM_AUTHTOK` until `pam_end`, hashing buffers), the
  kernel's socket buffers, and the text laid out for drawing while "show
  password" is on.

## X sessions (stage 3)

A session from `/usr/share/xsessions` (LXQt on AcornOS) goes through the
same steps as above, with the X server started in between
(`src/bin/reclogind/xserver.rs`). No `startx`, `xinit` or `xauth` is used.

1. Step 2: the X server program is `/usr/bin/X` when it exists (on AcornOS
   a link to the `Xorg` script, which runs the setuid wrapper
   `/usr/libexec/Xorg.wrap`), else `Xorg` on the worker's `PATH`. Neither:
   "No X server is installed.", before any password is asked for, and
   nothing is started.
2. Steps 3 to 6 as for Wayland: PAM, the console handed to the user
   (owner, group `tty`, 0620), `XDG_SESSION_TYPE=x11` in the PAM
   environment, the PAM session opened.
3. The access key: 16 bytes from the kernel's random source (`getrandom`)
   as an MIT-MAGIC-COOKIE-1, in an X authority file the worker writes itself
   with the user's file-system ids (so it is the user's), mode 0600, opened
   with `O_CREAT|O_EXCL|O_NOFOLLOW`: `$XDG_RUNTIME_DIR/reclogin-xauth-<random>`
   from the PAM environment, else `/tmp/reclogin-xauth-<uid>-<random>`. Two
   entries in libXau's format: family 256 (local) with the host name from
   `gethostname`, and family 65535 (any host) with an empty address, so that
   clients still find the cookie when the host name changes; each with the
   display number, the name `MIT-MAGIC-COOKIE-1` and the same cookie; each field a big-endian 16-bit length and its bytes, the family a
   big-endian 16-bit number. It is written first with an empty number (the
   server reads every cookie in its file whatever the number) and rewritten
   with the number once the server has said it.
4. Still before `ready_to_start`, so that a failure can be shown on the
   login screen: the worker forks the X server as the user (the worker's
   groups as after `pam_setcred`, the user's gid and uid; a later
   `setuid(0)` must fail), in a new session with the console as
   its controlling terminal and standard input, its working folder the
   home, the session's environment (as in step 8), and runs

   ```
   /usr/bin/X vt<N> -nolisten tcp -keeptty -novtswitch -auth <file> -displayfd 3
   ```

   No display number is given: with `-displayfd` the server takes the first
   free one and writes it to descriptor 3 (a pipe) once it accepts clients.
   `-keeptty`: the server would otherwise try to leave its terminal
   (`setpgid`, `setsid`); it turns this on by itself when a standard
   descriptor is its console, which ours is. `-novtswitch`: the server does
   not show the console that was shown when it started on reset and exit;
   `reclogind` decides which console is shown. No `-noreset` (XLibre
   refuses it). The server switches to console N itself; `reclogind` pauses
   the greeter on that switch as on any other.
5. Its standard output and error go to a pipe (which the user can write to
   as well); the worker writes its lines to the system log as
   `X server: <line>`, in constant memory: at most 64 KiB read per turn,
   lines cut at 1024 bytes, at most 50 lines a second and then one "N more
   lines dropped". The display number's pipe is read up to 32 bytes; more
   without a newline fails the start. The server writes its own full
   log itself: without root rights (the wrapper gives them up when every
   display device has a kernel driver) to `$XDG_DATA_HOME/xorg/Xorg.N.log`,
   else `~/.local/share/xorg/Xorg.N.log`; with root rights to
   `/var/log/Xorg.N.log`.
6. Waiting for the number, at most 30 seconds, watching for the server's end,
   a cancel and the worker's signals. A server that ends, says something
   that is not a number, or says nothing in time is stopped (below), the
   authority file removed, the console shown before it shown again if the
   server's console is still shown, the PAM session closed, and the greeter
   gets "The X server did not start." The log has the server's lines
   (above) and the reason.
7. Ready: `ready_to_start`, and the desktop's process as in step 8, with
   `DISPLAY=:<number>` and `XAUTHORITY=<file>` added and `XDG_SESSION_TYPE=x11`
   from PAM. The console is its standard input but not its controlling
   terminal: the X server holds that.
8. The worker waits for the desktop's process while passing the server's
   lines on. If the X server ends first, the desktop's process group gets
   SIGTERM (SIGKILL after ten seconds). When the desktop's process has ended,
   the X server gets SIGTERM, and SIGKILL after five seconds (then the console
   is put back in text mode, which the server had no chance to do); the
   authority file is removed. Then step 9 as for Wayland.

## The screens (stage 4)

`reclogin-greeter --render-png` draws each one from a sample state under
`assets/dev/` (see "Development switches"); the form (`form.rs`) knows what
is chosen, typed and focused, the screens (`screens/`) only draw it.

| Screen | Shown when | Sample state |
|---|---|---|
| Login (`login.rs`) | a user who is not logged in is chosen | `state-levitateos.json`, `state-acornos.json`, `state-*-wrong-password.json` |
| Choose a new password (`conversation.rs`) | PAM asks further questions | `state-acornos-expired-password.json` |
| Other user (`other.rs`) | the "Other user…" row is chosen: user name and password; the user name is cut to what a login name can be (no control characters, white space, `/` or `:`, no leading `-`, 128 characters) before `begin {typed_user}` | `state-acornos-other-user.json` (session menu open, two layouts) |
| No accounts (`other.rs`) | `State::users` is empty: a note in place of the list, the same form | `state-levitateos-no-users.json` |
| Guest (`prompt.rs`) | the Guest row is chosen: the text and "Start guest session"; the button sends `begin {guest}` | `state-acornos-guest.json` |
| Return (`prompt.rs`) | a logged-in user is chosen: the text and "Return to ...'s desktop"; the button sends `return_to {user}` | `state-levitateos-return.json`, `state-levitateos-return-refused.json` |

Returning and the power question are stage 5 (below). The return screen
adds "Session Plasma (Wayland), running since 19:10" from
`State::running`; a failed return shows under the button as an error line.
The guest's `begin` is stage 7 (below).

- **Session picker.** Only when two or more valid sessions exist. The label
  opens the menu "Installed desktops" with the session in use ticked: click,
  Enter or Space opens it; Up and Down move, Enter or Space or a click
  chooses, Escape or a click elsewhere closes. Choosing sends
  `choose_session` (and again with `begin`). The session shown for a user is
  the last one they used (`last_session_for`), else the first.
- **Last user and session** (`last.rs`): written when a session has really
  started (the worker said `started`) for a user of the list, with the
  session `login.rs` chose for it (the chosen one, else the first). A wrong
  password or a session that did not start changes nothing. `last_user` is
  preselected; with none the first user is.
- **Layouts.** With one layout the footer shows a plain label; with several
  all are shown, the one in use underlined. Click, or Enter or Space on it
  when focused, switches to the next: the greeter compiles that keymap, the
  "Typed with the ... layout." line follows, and `set_layout` is sent.
  reclogind keeps nothing: after a greeter restart the first layout is in use.
- **Focus order**: the list, the user name (Other user), the password, the
  session, the layout (several only), Log in; Tab and Shift+Tab, no trap.
  Choosing a user moves focus to the password, the guest or a logged-in user
  to the button, "Other user…" to the user name; Enter in the user name moves
  to the password. A wrong password keeps the user (or typed name) and the
  session.

## Other people and power (stage 5)

Written and compile-checked, never run: no login service was called.

**The helper.** `reclogind` is single-threaded on purpose (it forks the
greeter and applies a lock-down between fork and exec), and the D-Bus library
(`zbus`, pure Rust, blocking API) starts threads. So everything that talks to
the login service is a separate process, `reclogind --login-service`
(`src/bin/reclogind/loginhelper.rs`), started by `reclogind` with its end of
a socket pair as descriptor 3, the crate's own framing, `PR_SET_PDEATHSIG`
`SIGTERM`, an empty environment. It runs as root, connects to
`unix:path=/run/dbus/system_bus_socket` and talks only to
`org.freedesktop.login1`. The messages (`src/bin/reclogind/logind.rs`):
to the helper `refresh`, `activate {request, id}`, `power {request, action}`;
from it `report {available, sessions, power}` and `done {request, ok, text}`.

D-Bus calls, all on `/org/freedesktop/login1`, interface
`org.freedesktop.login1.Manager` unless said: `ListSessions`;
`org.freedesktop.DBus.Properties.GetAll("org.freedesktop.login1.Session")`
on each session's path (`Class`, `State`, `VTNr`, `Type`, `Desktop`,
`Timestamp`); `CanSuspend`, `CanReboot`, `CanPowerOff` (only `yes` shows the
action); `ActivateSession(s)`; `Suspend(false)`, `Reboot(false)`,
`PowerOff(false)`. Hibernate is never asked or offered. Signals: one match
rule, type signal and path namespace `/org/freedesktop/login1`, so every
signal of the manager, the sessions and the seat (`SessionNew`,
`SessionRemoved`, `PropertiesChanged`, `PrepareForSleep`, ...) wakes the
helper; it reads again 150 ms after the last one, on `refresh`, and every
five seconds whatever happens. A report is sent only when it changed. The
sessions reported are every session of class `user` whose state is not
`closing`, on any seat or on none (logged in over the network: `Remote`).
The logged-in marks of the list, returning and the foreground session use
only those on `seat0` with a console of their own; the power question names
everyone with a session (remote ones marked as such). A request the helper
does not answer within 15 seconds fails for the greeter, and the helper is
ended and started again.
With no bus (not up yet, or gone) it reports `available: false` and tries
again every two seconds. A method call that gets no answer in 25 seconds is
an error.

**If the helper dies** (or cannot be started) `reclogind` empties its view:
the screen shows no logged-in marks and no power actions, and logging in
still works. It is started again after 1 s, then 2, 4, ... at most 30 s; a
helper that lived a minute starts the next wait from 1 s again. A request
waiting for it fails with "The login service is not available."

**What the screen shows from it.** `State::users[].logged_in` for users with
a session, `others_logged_in` (everyone with a session, listed or not),
`power`, and the new `State::running` (user, the desktop's name, console,
start time) for the return screen. The desktop's name is the installed
session whose first `DesktopNames=` entry (else its id) equals the session's
`Desktop` property and whose kind equals its `Type`; else "Desktop
(Wayland)" and the like; `tty` is "Text console". A fresh `state` goes to
the greeter, a paused one included, whenever the view changes. After a
session reclogind started ended, the login service is asked again before the
list is trusted (`return_to_login`).

**Returning.** `return_to {user}`: `reclogind` picks the user's newest
session on `seat0` and asks the helper for `ActivateSession`. When that
succeeds the greeter gets `auth_result {ok: true}` and `pause`, the inputs
are revoked and the display is given up (`Daemon::release_display`, the same
steps as when the kernel asks for the console, which may have come first;
doing it twice is harmless). The console comes back the usual way
(`SIGUSR2`): the paused greeter gets `resume` and a fresh read. If the
service refuses, the greeter gets `auth_result {ok: false}` with "The
session could not be brought to the front." and the service's reason. If the
login console is still the one shown five seconds after a success, the
display is taken back with "The session did not come to the front."

**Power.** The footer's Sleep, Restart and Shut down send
`power {action, confirmed: false}`. With nobody logged in `reclogind` asks
the helper. With others logged in, the question is `reclogind`'s, not the
greeter's: for Restart and Shut down it answers `power_question {action,
people}` (who is logged in, remote ones marked), and `confirmed: true` is
honoured only for that action, within a minute of that question, and only
while nobody has logged in since; otherwise it asks again, whatever the
greeter sends. An accepted Restart or Shut down that has not happened 90
seconds later is given up and the greeter told. The greeter shows the
question screen (`screens/shutdown.rs`, `AcornShutDown.dc.html`): "Shut down for
everyone?" or "Restart for everyone?", who is logged in, "... anyway"
(sends `confirmed: true`) and "Cancel", the footer action marked. Sleep asks
nothing. `reclogind` refuses an unconfirmed Restart or Shut down while
anyone has a session, whatever the greeter sent, and refuses any action the
service does not allow now. A refusal or failure comes back as the new
message `power_refused {text}` (the service's reason) and shows as an error
line under the buttons. After a Restart or Shut down went through nothing
more is asked. Keyboard: Tab reaches the footer actions after "Log in"
(Enter or Space presses one); on the question the focus starts on "Cancel"
(Enter must not shut everyone's work down), Tab goes "... anyway", "Cancel",
the layout, the actions; Escape or "Cancel" leaves the question and puts
focus back on the action. The question also closes when the greeter is
paused or the others log out.

**The display at start and stop.** At start `reclogind` waits up to two
seconds for the helper's first report. If the console shown then belongs to
a running user session (a session whose `VTNr` it is, a text login on
console 1 included), it takes console 7 (graphics mode, switching by
signal) *without* showing it and without becoming the display's master, and
starts the greeter paused, as after a session start: the display and state
go to it when console 7 is shown (someone switches to it), and when that
session ends while its console is still the one shown and nothing else is,
`reclogind` shows console 7 (`Service::foreground`). The same rule serves a
session returned to. A person on another console is left there. When
`reclogind` stops, its restore (and the last-resort restore) shows the
console it started from only if console 7 is the one shown; otherwise the
display is not switched away from whatever is shown.

Protocol additions (version unchanged; every one has a default or is new):
`State::running` (`#[serde(default)]`), the type `RunningSession`, the
message `ToGreeter::PowerRefused {text}`. No message shape changed.

## The guest (stage 7)

As `docs/reclogin-guest.md` says, with the owner's decisions of 2026-11-15
(`src/bin/reclogind/guest.rs`). One fixed account, `guest`, uid and gid
60600, home `/run/reclogin/guest`, locked (`!*`), made by the sysusers line
in `dist/sysusers/reclogin.conf`, never at login. `guest = on|off` in
`/etc/reclogin.conf` (default `on`, `config.rs`).

The account is used only when it is exactly that: uid and gid 60600, that
home, no other account with uid 60600, and a shadow password starting with
`!` or `*`. Anything else (an old account called `guest`) means no Guest
entry, logged with the reason; no tmpfs is mounted over it and no process
of it is swept.

### Step by step

1. **reclogind starts.** It checks the account. When it is the guest, it
   takes the guest lock and starts the cleanup (below) in a child process,
   `reclogind --guest-cleanup --setid-walk`, which gets the lock as
   descriptor 3; also with `guest = off`. The main process stays
   single-threaded and reaps the child from its loop without waiting. The
   screen gets the Guest entry when the setting is on, the account is the
   guest, no cleanup child runs, the last cleanup did not fail and the
   start's search for set-id files finished clean (`guest::offered`, asked
   each time a state is built; the account check is read again only when
   `/etc/passwd`, `/etc/shadow` or `/etc/group` changed). A fresh state goes
   to the greeter whenever a cleanup child starts or ends. A failed
   cleanup runs again after 1 minute, then 2, 4, ... at most 30.
2. **`begin {who: guest}`.** Refused with "The guest session is switched
   off." (`guest = off`), "The guest session is not available." (no valid
   account) or "The guest session is still being cleared. Try again in a
   moment." (a cleanup runs or failed). A typed `guest` under "Other user…"
   fails like a wrong password, before PAM, and the worker refuses the name
   `guest` and uid 60600 in the normal `reclogin` service as well. Otherwise
   the session is chosen as for a person and a worker is started with
   `start_guest {session, console, wait}`; `wait` is set only for the
   automatic login, which comes while the start's cleanup runs and waits up
   to 110 seconds for it.
3. **The worker takes the guest lock** (`/run/reclogin/guest.lock`, `flock`,
   held to the end of its logout cleanup). Held by a guest session: "A guest
   session is already running." Held by a cleanup (or by a holder whose
   line cannot be trusted): "The guest session is still being cleared. Try
   again in a moment." The holder line is 32 bytes written in one call over
   the old one, never emptied, and trusted only while the process it names
   lives and is a worker or a cleanup child.
4. **The cleanup**, without the login service (below). A guest login only
   starts when it finished completely; otherwise the screen gets "The guest
   session is still being cleared. Try again in a moment."
5. **The home.** `/run/reclogin` (root, 0755) and `/run/reclogin/guest`
   (root, 0000); a tmpfs mounted there with
   `size=<a quarter of memory, at most 4 GiB>,mode=0700,uid=60600,gid=60600`
   and `nosuid,nodev`; checked to be a tmpfs (statfs), a mount (its device
   differs from `/run/reclogin`'s), the guest's, mode 0700 and empty. Then a
   child process that has fully become the guest (no groups; real,
   effective and saved uid and gid 60600; checked) copies `/etc/skel` and
   then `/usr/share/reclogin/guest-home/` in (folders, files, links; only
   the permission bits, never set-id ones; what the guest may not read is
   left out), so root never works on a path inside the guest's home. Steps
   3 to 5 are announced to reclogind as a step not to be cut short (`hold`).
6. **PAM** `reclogin-guest` for the user `guest`, `PAM_TTY` set, then
   authenticate and account. No conversation: a question from any module is
   refused (`Talk::silent`), which fails the login. A module that changes
   the user away from `guest` fails it too. Then `accepted`.
7. **The usual path** (`session::run`): console, PAM session, X or Wayland,
   the desktop, its end. Differences: after `pam_open_session` the PAM
   environment must hold `XDG_SESSION_ID` (the login service registered the
   session); without it the session is closed and the screen gets "The
   session could not be started." The desktop's process and the X server's
   process install no-new-privileges and a seccomp filter just before
   `exec` (`nosetid.rs`): no process of the session can make a set-user-ID
   or set-group-ID file anywhere, and no set-id program gains its rights in
   it (`su`, `sudo`, `doas`, `crontab`, `at`, `fusermount`, `pkexec` do not
   work; an X server that needs root because not every graphics card has a
   kernel mode-setting driver does not start). On LevitateOS the guest's
   `systemd --user` gets the same through
   `dist/guest/user@60600.service.d/50-reclogin-guest.conf`. The guest is
   never written to `last.json`.
8. **Logout.** When the desktop has ended the worker closes the PAM session
   (and `pam_end`), says `ended`, runs the cleanup again (which ends every
   process of the guest and unmounts the home), and only then exits;
   reclogind brings the login screen back when the worker has exited, as for
   anyone. After every guest worker, however it ended (killed included),
   reclogind starts the cleanup child again, the only place that asks the
   login service to end the guest: on LevitateOS the worker sits in the
   guest's own session scope and would be ended by its own request.

### The cleanup routine

One routine, run at reclogind's start, before every guest login and after
every guest logout. Each step is safe to repeat. The places are one list at
the top of `guest.rs`.

1. The guest lock stands for "the guest is logged in": a cleanup never runs
   while a guest worker holds it (reclogind starts no child then and tries
   again later; the worker runs it under its own hold).
2. Scheduled jobs first, so nothing starts a guest process after the sweep:
   `/var/spool/cron/guest`, `/var/spool/cron/crontabs/guest`, and every
   entry of uid 60600 in `/var/spool/at`, `/var/spool/cron/atjobs` and
   `/var/spool/cron/atspool`.
3. In the cleanup child only: `TerminateUser(60600)` through the one-shot
   `reclogind --end-user 60600` (at most five seconds; a courtesy). Then the
   uid sweep: a helper process takes no groups, the gid of `reclogin-sweep`,
   real and effective uid 60600 with saved uid 60601 (clearing the guest's
   user and user-session keyrings, which the kernel finds by the real uid),
   then real uid 60601 and effective uid 60600, checks every id and that no
   other process has uid 60601, and calls `kill(-1, SIGKILL)`. The kernel
   lets a process without `CAP_KILL` signal a target when the sender's real
   or effective uid equals the target's real or saved uid, so the helper
   reaches exactly the processes whose real or saved uid is 60600 (or 60601,
   nobody), and no guest process can signal it (its real and saved uid are
   60601). A helper that does not finish in three seconds is killed; the
   sweep repeats until `/proc` shows no process with real or saved uid 60600
   (zombies aside), for up to ten seconds, and fails after three helpers
   that did not finish. `reclogin-sweep` must be exactly uid 60601
   (`dist/sysusers/reclogin.conf`); without it the sweep and the cleanup
   fail.
4. Everything of the guest's mounted is detached: at or under
   `/run/reclogin/guest`, `/run/media/guest` and `/run/user/60600`, and a
   FUSE mount of the guest's (`user_id=60600`) under a shared place. Each
   mount is opened from `/` one name at a time without following links,
   checked to be the very mount (its id in `/proc/self/mountinfo`) and
   detached through `/proc/self/fd/N`; nothing else is ever unmounted.
5. Removed whole when present: `/run/reclogin/guest`, `/run/media/guest`,
   `/run/user/60600`, `/var/lib/systemd/linger/guest`,
   `/var/lib/elogind/linger/guest`, `/var/lib/AccountsService/users/guest`,
   `/var/lib/AccountsService/icons/guest`.
6. Everything of uid 60600 under `/tmp`, `/var/tmp`, `/dev/shm` and
   `/dev/mqueue` is removed (a folder of the guest's with all it holds).
7. The per-user journal files of uid 60600 (`user-60600*.journal*` in each
   machine folder of `/var/log/journal` and `/run/log/journal`) and
   systemd-coredump's dumps of uid 60600 in `/var/lib/systemd/coredump`.
8. System V shared memory, semaphores and message queues whose owner or
   creator is uid 60600 (`/proc/sysvipc/`) are removed.
9. At start only (`--setid-walk`): every mounted local file system that is
   written to and allows set-id programs (no `nosuid`, not read-only, not
   the kernel's own, not network or FUSE) is searched, each once and
   staying on it, for files and folders of uid 60600 with a set-id bit;
   the bits are taken away (through `/proc/self/fd/N` of a handle checked
   to be the same file). The search may take 60 seconds; longer, or a place
   it could not look at or fix, fails the check. When it finished clean the
   mark `/run/reclogin/guest-setid-checked` is written; until then no guest
   starts. It walks every home and all of `/var`, too long to do before
   each guest; the kernel filter of step 7 above is the guarantee and this
   search the backstop, once per boot.
10. The check: no such process, nothing of the guest's mounted (a FUSE mount
    of uid 60600 anywhere counts), none of the paths of steps 2 and 5,
    nothing of uid 60600 in the places of steps 2 and 6, no journal file or
    core dump of step 7, no such System V object (shared memory already
    marked for removal aside). Only then is it "finished completely".
    Otherwise the mark `/run/reclogin/guest-cleanup-failed` is written (and
    removed by the next clean pass), and the log says what is left.

Every file walk works from open folder handles (`openat`, `fstatat`,
`unlinkat`), never follows a link and never enters another mount. It has no
depth limit: it keeps only the handle of the folder it is in and climbs
back with `..`, checked to be the folder it came from; when a folder moved
meanwhile it begins again from its top (at most 64 times). The removal
walks of one cleanup, and its check, may take 30 seconds each. An entry
that cannot be looked at, opened or removed for any reason but "does not
exist" fails the check, unless it is someone else's mount point that root
may not look into.

### Files under dist/ for the guest

| File | Installed as | Design point |
|---|---|---|
| `dist/sysusers/reclogin.conf` (the `g guest`, `u guest` and `u reclogin-sweep` lines) | `/usr/lib/sysusers.d/reclogin.conf` | 1, 3, 6 |
| `dist/pam/reclogin-guest.levitateos`, `dist/pam/reclogin-guest.acornos` | `/etc/pam.d/reclogin-guest` | 4 |
| `dist/guest/50-reclogin-guest.limits.conf` | `/etc/security/limits.d/50-reclogin-guest.conf` | 3 |
| `dist/guest/50-reclogin-guest.sshd.conf` | `/etc/ssh/sshd_config.d/50-reclogin-guest.conf` | 4 |
| `dist/guest/50-reclogin-guest.rules` (an allow list: everything not on it is refused for the guest; no admin step) | `/usr/share/polkit-1/rules.d/50-reclogin-guest.rules` | 1 (decisions 3 and 4) |
| `dist/guest/user@60600.service.d/50-reclogin-guest.conf` (LevitateOS only) | `/usr/lib/systemd/system/user@60600.service.d/50-reclogin-guest.conf` | 1 |
| `dist/guest/reclogin-guest-notice` | `/usr/libexec/reclogin/reclogin-guest-notice`, mode 0755 | 5 |
| `dist/guest/guest-home/` | `/usr/share/reclogin/guest-home/` (the guest defaults tree; desktop recipes add their own files) | 2, 5 |

## Automatic login (stage 6)

`/etc/reclogin.conf` names an account in `autologin` (or `guest`) and
optionally a session in `autologin_session`; `src/bin/reclogind/autologin.rs`
does the rest. No countdown, no screen first: the session starts, and when it
ends the login screen appears, as after any logout.

### Step by step

1. `reclogind` starts as always (guest cleanup, login service, console,
   greeter). With the greeter up, `Daemon::start_autologin` runs once from
   `run`. Without an `autologin` line nothing happens. If a user session's
   console is in front at that moment (a restart of `reclogind` under a
   running desktop) it does not log anyone in.
2. It creates `/run/reclogin/autologin-done` (`create_new`, mode 0600, no
   symlink followed). If the file exists, this boot already tried: nothing
   more, no line. If it cannot be made, nothing is tried and the screen says
   so. `/run` is cleared at boot, so the next boot tries again.
3. The account: `guest` goes through the guest's own path (refused when
   `guest = off` or the account is not the guest; the cleanup and the home
   are the worker's, `guest::worker_login`). Any other name must be on the
   user list (`users::listable`: uid 1000 to 60000, a login shell, an
   existing home), so root and system accounts are refused here.
4. The session: `autologin_session` when installed; else the session that
   user used last (`/var/lib/reclogin/last.json`) when installed; else the
   first installed. None installed: refused.
5. `Daemon::start_login` (`login.rs`), the code `begin` ends with (console,
   worker), with the entry marked `Entry::Automatic`: the worker is sent
   `start_auto` instead of `start`.
6. The worker (`autologin::worker_login`) checks the account again itself
   (on the list, not root, not uid 60600, the name is what it was given),
   reads the session, and runs PAM's `reclogin-autologin` with no
   conversation (`Talk::silent`: any question from a module is refused).
   `pam_authenticate` and `pam_acct_mgmt` must succeed and the user must
   still be the same; an expired password is refused with its own line
   (changing it needs the login screen). Then the usual path, `session::run`.
7. A failure at any step is one line on the login screen: "Automatic login
   did not work." and the reason ("That account is not on the list of users,
   so it cannot log in this way.", "The account was refused.", "The chosen
   session is no longer installed.", the session's own refusals, ...). It
   comes as `auth_result` with `ok: false`, prefixed by `Entry::line`, also
   for the worker's own `failed`. After the session, or after the failure,
   nothing logs in again.

### Why it is not a way around the password

The auth line of `reclogin-autologin` is `pam_permit`: that service accepts
anyone. It is safe because it is reachable only like this: only `reclogind`'s
start-up sends `start_auto` (the greeter has no message that leads to it; the
worker's socket is private), only once per boot (the mark file, made before
anything is tried), only for the account in the root-owned
`/etc/reclogin.conf`, which the worker rechecks against the user list and
root. Anyone who can write `/etc/reclogin.conf` or `/etc/pam.d` is root
already. The account and session lines are those of `reclogin`, so locked or
expired accounts and `pam_nologin` still refuse. The comment in the PAM files
says nothing else may name the service.

### The PAM service and the installer

`dist/pam/reclogin-autologin.levitateos` and `.acornos` are installed as
`/etc/pam.d/reclogin-autologin` (`installed_files` in
`distro/builder/os-recipes/reclogin.rhai`). `dist/reclogin.conf` documents
both keys.

The installer's `autologin` command (`recipe-system-setup`,
`distro/builder/guest/installed/system-setup.sh`) writes `autologin = USER`
and `autologin_session = SESSION` into `/etc/reclogin.conf` when
`/usr/bin/reclogind` exists in the system it configures (other lines kept;
the two keys replaced in place or added), and the SDDM file otherwise. That
SDDM branch goes at the switch. `recsetup` (`plan.rs`) accepts a target that
has `reclogind` or `sddm` for `--autologin`.

### The guest, two connections

- **`TerminateUser`.** The guest cleanup's `ask_login_service_to_end_user`
  runs `reclogind --end-user UID` (`loginhelper::end_user`): a one-shot
  process (root only, signals unblocked, standard output closed) that
  connects to the system bus as the helper does (the same address and
  `connect_with`, a four-second call timeout), calls
  `org.freedesktop.login1.Manager.TerminateUser(u)` and exits. The cleanup
  waits at most five seconds (`END_USER_WAIT`), kills it after that, ignores
  any failure, and goes on to its own sweep. `reclogind` itself stays
  single-threaded.
- **A running guest session.** The guest is not in `State::users`, but the
  login service lists its session: `form::guest_running` (greeter) is true
  when `State::running` has a session of user `guest`. The Guest row then
  shows "LOGGED IN" like a user's, the Guest screen reads "Back to the guest
  session." with a "Return to the guest session" button (`return_to` with
  the name `guest`, handled by the existing `on_return_to`; no second guest
  starts), the session line, and under the button a quiet underlined "End
  the guest session". That one sends the new message `ToDaemon::EndGuest`;
  `Daemon::on_end_guest` (`guest.rs`) calls `guest::end_running_session()`
  and answers `auth_result` (`ok: true` when the worker was asked to end,
  else the reason); the next state, which no longer has the guest running,
  puts the normal Guest screen back.

Protocol: one new message, `ToDaemon::EndGuest`; no existing shape changed.
Between `reclogind` and its worker: `ToWorker::StartAuto` (`start_auto`).

## Keyboard, touch, several monitors, idle (stage 8)

All in the greeter. Drawing: `screens/keyboard.rs` (the login with the
keyboard open, `LevitateKeyboard.dc.html`) and `screens/second.rs` (what the
other monitors show, `LevitateSecondScreen.dc.html`). The keys' state and
letters: `osk.rs`. Devices: `kms.rs`, `input.rs`. The rest is `watch.rs`.

**The on-screen keyboard.** The footer's "On-screen keyboard" opens it. While
it is open the user list is the chosen person with "Someone else" (on
"Other user…": that row with "Choose from the list"; on a PAM question:
"Someone else" cancels, as on the question screen), the headline is 56 units,
the password field and "Log in" share a row, the footer and the session
picker are not drawn, and five rows of keys 56 units high take the lower part
(digits; `Q` row; `A` row with Enter; Shift, `Z` row, Delete; the symbols key,
the layout key, space, "Hide keyboard"). It types into the form's focused
field (password, user name, or a PAM answer) as a physical key does: it hands
`Form::key` the same `Key` values, so what a physical key press does still
works at the same time. A pressed key is drawn pressed until the frame after
the pointer or finger is up. Not on the guest and return screens (no field),
not on the power question; "Hide keyboard" closes it. The delete key does not
repeat when held.

The letters are not a fixed table: the keymap the greeter compiled for the
current layout (`input::Input::key_table`) is asked, with
`xkb_keymap_key_get_syms_by_level` for layout group 0, which character each
of the 38 key positions gives at levels 1 to 4: evdev codes 2-11 (digits),
16-25, 30-38 and 44-52 (`Z` to `.`). A German layout shows z and y where
that layout has them. Shift is one-shot (the next character); pressed twice
it locks ("Shift lock", drawn filled), a third press releases. Shift shows
the layout's second level. The digit row types digits (on a layout that has
them under Shift, such as French, they are shown plain). The symbols key
("!#1", then "abc") switches the three letter rows to the layout's third
level (Shift: fourth) when that level is a symbol layer: at least 14 of the
28 keys have a character there and three quarters of those are not letters.
Otherwise (US, German, most layouts: AltGr gives letters or nothing) it shows
a **fixed table of common symbols**: page one `!@#$%^&*()`, `-_=+[]{};`,
`:'"\|/<>?`; with Shift a second page (`~`,.€£¥°§±` and Latin-1 signs). The
layout key switches the layout like the footer's switcher (nothing happens
with one layout). The keymap is read again whenever the layout changes.

**Touch.** libinput touch-down, motion and touch-up (the first finger down;
others are ignored until it is lifted) drive the pointer logic: a touch is a
click at that point (`Happening::Touch` becomes `Press`), and the hardware
and drawn pointers are hidden until the mouse moves. A touch on the password
or user name field opens the keyboard when no physical key has been pressed
since the greeter started. Touch coordinates are mapped onto the primary
screen (libinput cannot tell which monitor a touch screen belongs to).

**Several monitors.** Every connected connector gets a mode and two buffers,
each on a pipeline of its own. The primary is the first connected built-in
panel (eDP, LVDS, DSI), else the first connected screen in the kernel's
order; it shows the login and holds the pointer (the pointer is clamped to
its size and the hardware cursor is set on its pipeline only). Every other
screen shows the background art with the mark, the OS name and "Log in on the
other screen.", drawn once for a screen and redrawn only when the brand's
name or that screen's size differs from what it shows. Each screen is
scaled by itself from the 1920x1080 design. **A plugged or removed screen is
noticed by looking at the connectors again every three seconds**
(`Screen::rescan`, `get_connector` without probing the hardware, so it reads
what the kernel's own hotplug handling keeps): the kernel announces it by a
uevent on a netlink socket, which the greeter's lock-down refuses to open
(no new sockets), and the display handle carries only page-flip events.
When the primary goes, the login moves to the next screen (the frame, the
pointer's box and the drawn login follow its size) and the former primary
shows the other-screen picture. When the last screen goes the old state is
kept. Screens found while the screens are off are drawn when they wake.

**Idle.** After `idle_seconds` (from `reclogin.conf`; unset or 0 is never)
without a key, pointer motion or touch the greeter sets the connector
property `DPMS` of every screen to Off and stops drawing. Any input sets it
On again and is not passed to the form (a held key's repeat is dropped as
well). A connector without a `DPMS` property has its pipeline disabled
(`set_crtc` with no buffer) instead, and the mode is set again by the next
frame. After 60 seconds without input a typed password (also a PAM answer
being typed) and a typed user name are cleared, with the line "The password
was cleared after a minute without input." (and its user-name variants). A
console switch away and back counts as a fresh idle period.

`reclogin-greeter --render-png out.png --brand-dir assets/levitateos --state
assets/dev/state-levitateos-keyboard.json --at '2026-11-15 21:42'` draws the
keyboard (`state-*-keyboard.json`; `-keyboard-shift.json` with Shift;
`state-levitateos-keyboard-de.json` a German layout;
`-keyboard-symbols.json` the symbols page with Shift lock). A sample's
`keyboard`, `shift` (`once`/`lock`) and `symbols` set it up.
`--second-screen` draws the other monitors' picture.

## Starting, restarting, giving up (stage 9)

- The init system starts `reclogind` (the files under `dist/`). It takes
  console 7, the display, and starts the greeter.
- A greeter that exits, closes its socket or cannot be reached without
  reclogind having asked it to go is replaced at once. A greeter that
  cannot even be started (the program or the account `reclogin` missing)
  counts the same. A greeter that goes because reclogind asked it to (`quit`,
  a session taking over: whoever stops it calls `Supervisor::stopping`
  with its process id, which `stop_greeter` does) never counts. Starting a
  greeter again after a session: `Supervisor::launch_until_up` (starts it,
  sends `hello`, the display and the state, counts failures).
- A greeter replaced while console 7 is not shown gets `hello` only, and the
  display and the state when the console is shown again (it would otherwise
  fail to set the mode and count as a crash).
- Three failures within sixty seconds: reclogind gives up. It writes the
  reason to the system log (syslog, facility daemon) and to standard error,
  gives the display up, puts console 7 back in text mode with its keyboard
  mode, writes on console 1 "The login screen stopped: it failed to start
  three times within a minute. Log in here." (with what was missing in
  brackets when the greeter could not be started) and a newline, shows
  console 1, and exits with status **3**. Neither init service starts it
  again on that status.
- SIGTERM or SIGINT (and SIGHUP): a login not yet running is cancelled and
  waited for (at most fifteen seconds, see "Logging in"; a running desktop
  goes on), `quit` to the greeter, two seconds, then SIGKILL; the display
  given up, console 7 back in text mode with its keyboard mode, the console
  reclogind started from shown again; exit 0.
- Every other signal whose default would end reclogind (SIGALRM, SIGVTALRM,
  SIGPROF, SIGIO, SIGPWR, SIGSTKFLT, the real-time signals) gets a handler
  that does nothing; a handler rather than "ignore", because exec keeps
  ignored signals and the greeter, the X server and the desktop must start
  with the defaults. SIGUSR1 and SIGUSR2 are the console-switch signals.
- **Sending to the greeter never blocks reclogind.** Its end of the socket is
  non-blocking; a message that does not fit waits in a queue (at most 64)
  sent from the main loop. A greeter whose queue has not moved for five
  seconds, or whose queue is full, is hung: killed and counted as a failure
  like a crash.
- **Liveness.** A greeter has a minute from getting its display and state
  to say `ready` (fonts and the first mode set are slow on slow machines);
  one that has not is hung. From `ready` on, while console 7 is shown,
  reclogind sends `ping {seq}` every three seconds (one at a time); a
  greeter that has not answered with `pong {seq}` within ten seconds is
  hung: killed and counted as a failure. The five-second limit on a queue
  that does not move also counts from `ready`. While console 7 is not shown
  none of it runs.
- If reclogind panics or gets a fatal signal (SIGSEGV, SIGBUS, SIGILL,
  SIGFPE, SIGABRT, SIGQUIT, SIGSYS, SIGTRAP, SIGXCPU, SIGXFSZ), a panic hook
  and signal handlers give the console back from numbers recorded when it
  was taken. Every other early return drops the console, which gives it
  back. SIGKILL cannot be caught: `reclogind --reset-console 7`, which the
  init services run after reclogind has ended (systemd: `ExecStopPost`, run
  however it ended; OpenRC: the wrapper `reclogind-run`, below), gives
  console 7 back if it is still in graphics mode and shows console 1.
- Console switches the greeter asks for (Ctrl+Alt+F1 to F12) go to consoles
  1 to 12 only. On `pause` the greeter clears a half-typed password and says
  so when the screen is shown again; a password already sent to reclogind is
  not affected.

Exit statuses: 0 stopped on request, 1 an error, 2 bad arguments, 3 gave up.

## The greeter's lock-down

Applied by reclogind in the greeter's process between fork and exec
(`src/bin/reclogind/greeter.rs`, `sandbox.rs`), so the greeter never runs
unconfined and cannot skip it. `--no-sandbox` turns off the items marked *.

| Item | Where | What |
|---|---|---|
| Account | fork–exec | `reclogin`, no supplementary groups. |
| No capabilities* | fork–exec | Bounding and ambient sets cleared while still root; the account change clears permitted and effective; the inheritable set is cleared after it and all three are checked to be empty. |
| No-new-privileges | fork–exec | Always. A later `setuid(0)` must fail, or the greeter does not start. |
| No network* | fork–exec | A new network namespace with nothing in it (left out, with a log line, on a kernel without network namespaces). |
| No core dumps* | fork–exec | Core size limit 0. The greeter also marks itself not dumpable at its start (exec resets that flag, so only it can), which also keeps other processes of its account out of its memory. |
| Limits* | fork–exec | 256 open files, 32 processes and threads for the account. |
| Nothing inherited | fork–exec | Descriptors 0 (`/dev/null`), 1 and 2 (reclogind's output and error) and 3 (the socket) only; every other one is closed at exec. Empty environment except `RECLOGIN_SOCKET`. Always. |
| Parent death | fork–exec | SIGTERM when reclogind dies, set after the account change (which clears it); a greeter whose reclogind already died does not start. |
| Files (Landlock)* | fork–exec | Every right the kernel's Landlock knows is handled; only these are allowed, read-only: the brand folder and `../fonts` beside it; `/usr/share/X11/xkb`, `/etc/xkb`; `/etc/localtime`, `/usr/share/zoneinfo`; `/dev/input` (listing only); `/sys`, `/run/udev/data`, `/etc/udev/udev.conf`, `/usr/share/libinput`, `/etc/libinput`, `/usr/share/libwacom`, `/etc/libwacom` (libinput and its udev library); the library folders `/lib`, `/lib64`, `/usr/lib`, `/usr/lib64`, `/usr/local/lib`, `/usr/local/lib64`; `/etc/ld.so.cache`, `/etc/ld.so.preload` (glibc), `/etc/ld-musl-x86_64.path` (musl). Execute on two files only: the greeter program and the loader its ELF header names (`/lib64/ld-linux-x86-64.so.2` on glibc, `/lib/ld-musl-x86_64.so.1` on musl). Nothing writable. Paths missing on a system are left out. With ABI 4 TCP is refused too; with ABI 6 abstract sockets and signals outside the greeter. Without Landlock the greeter still starts and reclogind logs that it runs without it. Handles reclogind passes (display, input devices) were opened outside the rules and keep working. |
| No new sockets* | the greeter's start | A seccomp filter (x86_64) the greeter installs right after the checks below, before it reads anything from reclogind or from input: `socket`, `socketpair`, `connect`, `bind`, `listen`, `accept`, `accept4` fail with `EACCES`, so Unix sockets in the file system (the system bus, the log sockets) are out of reach too; `sendmsg`/`recvmsg` on the inherited socket keep working; calls of another ABI (32-bit, x32) end the process. Not for `--render-png`. |
| Checked by the greeter | its start | Refuses to run as root; refuses to run without no-new-privileges unless started with `--no-sandbox`. |

## Files under dist/

| File | For |
|---|---|
| `dist/systemd/reclogin.service` | LevitateOS. After `systemd-logind`, `dbus`, `systemd-user-sessions`; conflicts with `getty@tty7`; `Alias=display-manager.service`. `Restart=on-failure` but not after status 2 or 3, at most three starts a minute; `KillMode=mixed` so only reclogind gets SIGTERM; `TimeoutStopSec=30` (the stop takes up to fifteen seconds plus the greeter's two); output and errors to the journal; `ExecStopPost` runs `reclogind --reset-console 7`. |
| `dist/openrc/reclogin` | AcornOS. Needs `elogind` and `dbus`, provides `display-manager`. Starts the wrapper `/usr/libexec/reclogin/reclogind-run` in the foreground, backgrounded and tracked by OpenRC (pid file), never respawned: supervise-daemon cannot leave out a respawn for one exit status. Output to syslog through `logger`. `retry="TERM/30/KILL/5"`. `stop_post` kills a reclogind left behind by a killed wrapper (by the pid the wrapper wrote, never by name) and runs `reclogind --reset-console 7`. |
| `dist/openrc/reclogind-run` | AcornOS, installed as `/usr/libexec/reclogin/reclogind-run` (mode 0755). OpenRC runs `stop_post` only on a requested stop; the wrapper runs reclogind, passes SIGTERM, SIGINT and SIGHUP on, and after reclogind ended for whatever reason runs `reclogind --reset-console 7` and exits with reclogind's status. Writes reclogind's pid to `/run/reclogin/reclogind.pid`. |
| `dist/sysusers/reclogin.conf` | The account `reclogin`: a system account, no shell, home `/`, no extra groups. The guest account and group (see "The guest"). |
| `dist/guest/` | The guest's other installed files (see "The guest"). |

| `dist/reclogin.conf` | The default `/etc/reclogin.conf`: every key, commented out. |
| `dist/recipe/login-screen.rhai` | The `login-screen` group recipe for both OSes, prepared and not active: it installs the pinned `login-screen` prebuilt and enables the service. `dist/recipe/SWITCH.md` says how it replaces SDDM. |

Both services start `/usr/bin/reclogind` (OpenRC through the wrapper), and
reclogind starts `/usr/bin/reclogin-greeter` beside it. The OS builder
compiles the crate per OS in that OS's apps-build root, against its libpam,
libinput and libxkbcommon (`distro/builder/src/login_screen.rs`), and lays
out the tree with every file's mode in one list (`installed_files` in
`distro/builder/os-recipes/reclogin.rhai`): the programs in `/usr/bin`,
`/etc/pam.d/reclogin`, `reclogin-guest` and `reclogin-autologin`, `/etc/reclogin.conf`,
`/usr/lib/sysusers.d/reclogin.conf`, the brand folder `/usr/share/reclogin`
(`brand.json`, `background.png`, `fonts/` with the `OFL-*.txt` licences,
`guest-home/`), the guest's files where "Files under dist/ for the guest"
says, and the systemd unit with its `display-manager.service` link or the
OpenRC service with its wrapper. The PAM files (`dist/pam/`) are described under "Logging
in".

## Not done

Everything of `docs/reclogin-plan.md` after stage 1 except stages 2, 3, 4, 5 and 9:

- stage 2 (above) is written and compile-checked, never run: no PAM module
  of either OS was called, no session was started, no console handed over.
  Not seen on a real system: how `pam_systemd` and `pam_elogind` take
  `PAM_TTY=ttyN` and the `XDG_*` values; whether a Plasma or LXQt session
  started this way gets its devices from logind; whether `/run/user/<uid>/bus`
  appears within three seconds on LevitateOS; whether `pam_unix`'s questions
  during an expired-password change look as assumed (its
  "Current password:" question is asked again after "Current password
  accepted."); the AcornOS PAM file against a system whose text login is
  busybox's `login`, which does not use PAM at all (so "what the text login
  accepts" there means the same shadow file through `pam_unix`, not the same
  program; busybox does not enforce password ageing, `pam_unix` does);
  whether a module of either OS refuses `PAM_TTY=tty8` and above during
  authentication (`pam_securetty` for a typed root, where it is listed); no
  process was killed on purpose to see a desktop ended after its worker, a
  worker outlive reclogind, or a console switch fail. No shake on a wrong
  password: the screen is drawn still;
- stage 3 (above) is written and compile-checked, never run: no X server
  was started by it. What it rests on was read in XLibre 25.1.11's source
  (`xorg-wrapper.c`, `lnx_init.c`, `xf86Init.c`, `connection.c`, `auth.c`,
  `xf86Helper.c`) and the try of section 10 of the technical design, not
  seen with this code: that the wrapper accepts a console as standard input
  when the process is also that console's session leader; that `-keeptty`,
  `-novtswitch` and `-displayfd` behave in the AcornOS build as in that
  source; whether the AcornOS server gives up root rights (all display
  devices with kernel drivers) and then opens the console and the display
  devices as the user; whether a desktop whose standard input is the console
  without it being its controlling terminal minds that; that clients find
  the key by the host name (a host name changed during the session breaks
  new connections, as with any family-local entry; no wildcard entry is
  written). Console N shown with nothing on it, between the end of the X
  server and `reclogind` showing console 7, is expected;
- stage 4 (above) is written and compile-checked, never run: only
  `--render-png` drew it. The renders are compared with the design files by
  eye and by their markup, not pixel by pixel. Not drawn as the designs do:
  the key caps of the no-accounts note use the sans face
  (no monospace font is in the brand folder); the session menu has no
  scrolling for more sessions than fit. Not confirmed: the AcornOS
  keyboard when only `keymap=` in `/etc/conf.d/loadkeys` was set (it is not
  an XKB layout, so the layout is `us`); how the real Plasma and LXQt session
  files pass `Exec`/`TryExec` (checked against the Desktop Entry
  specification only); a guest is not offered when no account is listed;
- stage 5 (above) is written and compile-checked, never run: the helper
  never talked to logind or elogind, nothing was returned to, no power
  action was asked, the question screens were only rendered. Assumed, not
  seen: that both services answer `ListSessions`, `GetAll` on the session
  interface (`Class`, `State`, `VTNr`, `Type`, `Desktop`, `Timestamp` in
  microseconds), `CanSuspend`/`CanReboot`/`CanPowerOff` and
  `ActivateSession` as the try of section 10 showed; that a session
  registered by the PAM module carries the `Desktop` the worker set, and
  that elogind sets `Timestamp` and emits `PropertiesChanged` (else the
  five-second re-read carries it); that `ActivateSession` switches the
  console by `VT_ACTIVATE`, so `SIGUSR1`/`SIGUSR2` reach `reclogind` as for
  any switch; that `Reboot`/`PowerOff` from root with `interactive=false` is
  not refused by inhibitors on these images (when it is, the service's
  reason is shown). Not done: a session without a console (`VTNr` 0) cannot
  be returned to (said on the screen); only the newest session of a user is
  returned to; no Hibernate; a power call that hangs
  holds the helper (not `reclogind`) for up to 25 seconds, and the greeter
  shows nothing while it waits; after Sleep nothing on the screen says the
  machine woke; a restart or shut down leaves no "going down" screen;
- automatic login (stage 6, "Automatic login" above) is written and
  compile-checked, never run: no worker was started, `reclogin-autologin`
  was not called by any PAM module of either OS, nothing was logged in. Not
  seen: `pam_permit` plus the account and session lines of each OS's own
  `reclogin` file in `reclogin-autologin` (AcornOS's `base-account` and
  `base-session` against that file's include lines, as for the guest);
  whether the login service registers a session opened with no password
  conversation like a normal one; whether wallets without a password
  (`pam_kwallet5`, `pam_gnome_keyring` with `auto_start`) behave in the
  session (they cannot unlock, so the desktop asks later). Not done: the
  greeter is already up when the worker starts, so the login screen can
  show for the moments the worker takes (starting before the greeter would
  need `login.rs` to work without one); no countdown or way to cancel
  (decision 2); a user whose home is encrypted or mounted by their password
  is not supported; a failed automatic login says why in one line and the
  logs (journal or syslog) say more;
- stage 7 (above) is written and compile-checked, never run: no tmpfs was
  mounted, no sweep run, no file removed, no PAM module called. 
  `reclogind --end-user 60600` calling `TerminateUser` is written, never
  run (whether logind and elogind accept it from the one-shot with no
  session of its own, and whether the dbus daemon is up at `reclogind`'s
  start; if not the call fails within moments and the sweep goes on);
  `state.rs` still offers Guest from the setting
  as known at start (main.rs gives it the setting and the account), not from
  `guest::offered`, so a cleanup that fails later hides nothing until
  reclogind restarts (a guest attempt still gets the plain line). The
  Guest row marked as logged in, the return to a running guest session and
  "End the guest session" are written ("The guest, two connections") but
  only compile-checked, never rendered or run: the quiet button has no
  keyboard focus (pointer and touch only); a guest session of this
  reclogind's own worker cannot be reached from the screen anyway (no greeter
  runs while it does), so this is for one that outlived a restart of
  `reclogind`. Step 1 of the cleanup asks the guest lock, not the login
  service: a guest session whose worker and reclogind both died is swept at
  the next start. Not seen on a real system: `pam_succeed_if`, `pam_permit`,
  `pam_limits`, `pam_unix` in AcornOS's linux-pam; `pam_systemd` and
  `pam_elogind` putting `XDG_SESSION_ID` into the PAM environment; whether
  `DenyUsers` in `sshd_config.d` is read on AcornOS and combines with a
  `DenyUsers` of the main file; polkit taking an empty admin list as
  "nobody"; the notice shown by Plasma and LXQt; `/bin/bash` on AcornOS (the
  session falls back to `/bin/sh` when it is missing; a terminal opened in
  the session uses the account's shell);
- stage 8 (above) is written and compile-checked, never run: no key was
  pressed on the on-screen keyboard, no touch screen was used, no second
  monitor was plugged, no screen was switched off; only `--render-png` drew
  it. Not seen: that `DPMS` works on a driver's connector for an
  unprivileged master (and what a driver without it does with a disabled
  pipeline); that the connectors' state read without probing changes after a
  hotplug within the three seconds on every driver (analog outputs are
  polled by the kernel's own work, which may be off); that a CRTC is free
  for every extra screen (a screen without one is left dark and logged);
  that the keymap's levels read as described on layouts other than `us` and
  `de` (those two were drawn). Not done: the keyboard on the guest and
  return screens; the session picker and the footer while it is open; key
  repeat for Delete; sliding a finger from one key to another (a key types
  when the finger goes down); a touch screen on a monitor other than the
  primary (touch is mapped onto the primary); input devices plugged in after
  start (libinput still lists `/dev/input` once);
- stage 9 (above) is written and compile-checked, never run: no greeter
  was crashed on purpose, no lock-down item was seen working on either
  kernel, the services were not installed, and the Landlock path list was
  not checked against what libinput, its udev library and the loaders
  really open;
- shipping (stage 10): the per-OS build, the prebuilt `login-screen` and the
  prepared group recipe are written, never run; nothing is published and
  SDDM is still the login screen of both OSes (`dist/recipe/SWITCH.md`).

**Nothing here was run on a console or a display.** It was compile-checked;
the only thing run was `--render-png`.
