# reclogin

One login screen for LevitateOS and AcornOS, in Rust. It replaces SDDM on
both systems with the same screen and behaviour; only the brand (name,
colours, mark, background) differs. It draws straight to the kernel's
display, with no X server, compositor or toolkit under it. The design is in
the LevitateOS repository: `docs/reclogin.md`, `docs/reclogin-technical.md`,
`docs/reclogin-plan.md` and the screens in `docs/reclogin-design/`.

Status: **stages 1, 2, 3, 4 and 9 of the plan, written and compile-checked,
never run** (see "Not done").

## The two programs

| Program | Runs as | Does |
|---|---|---|
| `reclogind` | root | Takes console 7 and puts it in graphics mode, with console switching by signal. Opens the display device and is its master while console 7 is shown. Starts the greeter. Opens input devices when the greeter asks (only `/dev/input/event*` character devices, read-only, non-blocking) and revokes them when the console is left. Reads the users, sessions and keyboard layout. Gives everything back at exit (text mode, keyboard mode, the console it started from). |
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
| `/etc/reclogin.conf` | reclogind | `key = value` lines: `autologin`, `autologin_session`, `guest` (`on`/`off`, default `on`), `idle_seconds`. Unknown keys are reported and ignored. Stage 1 uses `guest` and `idle_seconds` only (passed to the screen). |
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
    [--at '2026-11-15 21:42'] [--typed 12]

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
`ready_to_start {console, x_server}`, `started {console, desktop}` (the
desktop's process group), `ended {status}`.

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
   root included; the guest is stage 7) and the session (installed; else
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
the worker is gone. A worker that has not exited fifteen seconds after being
asked is logged loudly and killed, and reclogind gives its console back
itself (text mode, hang-up, root, `tty`, 0620). The greeter sends `cancel` at most
once per attempt, then shows "Cancelling…" and stays busy, sending no
`begin`, until that `auth_result` (or the attempt's own, if it crossed the
cancel) arrives; questions and messages that arrive meanwhile are dropped.

A worker that goes away while its desktop starts or runs (it crashed, was
killed) leaves the desktop to reclogind: it ends the desktop's and the X
server's process groups (reported in `ready_to_start` and `started`;
SIGTERM, SIGKILL after ten seconds, without stopping its loop), gives the
console back, logs it, and only then shows the login screen. A panic in the
PAM conversation is caught and refuses the conversation.

The reverse: once its desktop runs, the worker does not need reclogind. It
stops asking for a signal when reclogind dies and ignores the loss of its
socket, keeps waiting for the desktop and closes the session by itself when
the desktop ends. So stopping, restarting or losing reclogind leaves running
desktops alone; before the desktop runs, losing reclogind ends the login.
A restarted reclogind knows nothing of such a session: listing those
sessions (and returning to them) is stage 5's job, through the login
service. When reclogind is stopped it ends a login that is not yet running
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
   from the PAM environment, else `/tmp/reclogin-xauth-<uid>-<random>`. One
   entry in libXau's format: family 256 (local), the host name from
   `gethostname`, the display number, the name `MIT-MAGIC-COOKIE-1`, the
   cookie; each field a big-endian 16-bit length and its bytes, the family a
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
5. Its standard output and error go to a pipe; the worker writes every line
   to the system log as `X server: <line>`. The server writes its own full
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

The guest (stage 7) and returning (stage 5) are not built in `reclogind`: the
guest is refused by `login.rs` and `return_to` is answered in `main.rs`, both
with an `auth_result` the screen shows as its error line.

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
- **Liveness.** While console 7 is shown reclogind sends `ping {seq}` every
  three seconds (one at a time); a greeter that has not answered with
  `pong {seq}` within ten seconds is hung: killed and counted as a failure.
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
| `dist/sysusers/reclogin.conf` | The account `reclogin`: a system account, no shell, home `/`, no extra groups. |

Both services start `/usr/bin/reclogind` (OpenRC through the wrapper);
where stage 10 installs it may change that path. The PAM files (`dist/pam/`) are described under "Logging
in".

## Not done

Everything of `docs/reclogin-plan.md` after stage 1 except stages 2, 3, 4 and 9:

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
  the Return screen leaves out "Session ..., running since 19:10" (the state
  has no start time); the key caps of the no-accounts note use the sans face
  (no monospace font is in the brand folder); the session menu has no
  scrolling for more sessions than fit. Not confirmed: the AcornOS
  keyboard when only `keymap=` in `/etc/conf.d/loadkeys` was set (it is not
  an XKB layout, so the layout is `us`); how the real Plasma and LXQt session
  files pass `Exec`/`TryExec` (checked against the Desktop Entry
  specification only); a guest is not offered when no account is listed;
- who is logged in, returning to a session, power actions (stage 5; the
  system bus is used from there, at `/run/dbus/system_bus_socket`). This
  includes sessions left running by an earlier reclogind: a restarted one
  knows nothing of them until stage 5 lists them through the login service;
- automatic login (stage 6), the guest (stage 7);
- the on-screen keyboard, several monitors, idle switch-off, input devices
  plugged in after start (stage 8);
- stage 9 (above) is written and compile-checked, never run: no greeter
  was crashed on purpose, no lock-down item was seen working on either
  kernel, the services were not installed, and the Landlock path list was
  not checked against what libinput, its udev library and the loaders
  really open;
- the per-OS build and shipping (stage 10).

**Nothing here was run on a console or a display.** It was compile-checked;
the only thing run was `--render-png`.
