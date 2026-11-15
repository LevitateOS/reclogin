//! A try for reclogin: can a program with no rights draw the login screen
//! straight to the kernel's display and read input, with devices opened for
//! it by a root program? `spike daemon SECONDS` is the root part; it starts
//! itself as `spike greeter SECONDS` under uid 65534.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{connector, Device as ControlDevice};
use drm::Device;

const VT_OPENQRY: u64 = 0x5600;
const VT_GETSTATE: u64 = 0x5603;
const VT_ACTIVATE: u64 = 0x5606;
const VT_WAITACTIVE: u64 = 0x5607;
const KDSETMODE: u64 = 0x4B3A;
const KDGKBMODE: u64 = 0x4B44;
const KDSKBMODE: u64 = 0x4B45;
const KD_TEXT: i32 = 0;
const KD_GRAPHICS: i32 = 1;
const K_OFF: i32 = 4;

static FRAME_PNG: &[u8] = include_bytes!("../frame.png");
static FONT_TTF: &[u8] = include_bytes!("../font.ttf");

fn log(who: &str, text: &str) {
    eprintln!("[{who}] {text}");
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

fn ioctl_int(fd: RawFd, request: u64, value: i32) -> std::io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value as libc::c_long) };
    if rc < 0 { Err(errno()) } else { Ok(()) }
}

fn keep_across_exec(fd: RawFd) {
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
}

fn daemon(seconds: u64) {
    let who = "root part";
    log(who, &format!("uid {} euid {}", unsafe { libc::getuid() }, unsafe { libc::geteuid() }));
    let tty0 = OpenOptions::new().read(true).write(true).open("/dev/tty0").expect("open /dev/tty0");
    let mut state = [0u16; 3];
    if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_GETSTATE as _, state.as_mut_ptr()) } < 0 {
        log(who, &format!("VT_GETSTATE failed: {}", errno()));
    }
    let old_vt = state[0] as i32;
    let mut free_vt: i32 = 0;
    if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_OPENQRY as _, &mut free_vt) } < 0 || free_vt <= 0 {
        log(who, &format!("no free console: {}", errno()));
        std::process::exit(2);
    }
    log(who, &format!("the display is on console {old_vt}; taking free console {free_vt}"));
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(format!("/dev/tty{free_vt}"))
        .expect("open the free console");

    // The display device.
    let mut card = None;
    for n in 0..4 {
        let path = format!("/dev/dri/card{n}");
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => {
                log(who, &format!("opened {path}"));
                card = Some(Card(file));
                break;
            }
            Err(e) => log(who, &format!("{path}: {e}")),
        }
    }
    let Some(card) = card else {
        log(who, "RESULT: no display device (/dev/dri/card*) could be opened");
        std::process::exit(3);
    };
    if let Ok(driver) = card.get_driver() {
        log(who, &format!("display driver: {}", driver.name().to_string_lossy()));
    }

    // The input devices.
    let mut inputs: Vec<File> = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/dev/input") {
        let mut names: Vec<_> = dir.flatten().map(|e| e.path()).collect();
        names.sort();
        for path in names {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with("event") {
                continue;
            }
            match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path) {
                Ok(file) => inputs.push(file),
                Err(e) => log(who, &format!("{}: {e}", path.display())),
            }
        }
    }
    log(who, &format!("opened {} input devices", inputs.len()));

    // Take the console and the display.
    let tty_fd = tty.as_raw_fd();
    let mut old_kb: i32 = 3;
    unsafe { libc::ioctl(tty_fd, KDGKBMODE as _, &mut old_kb) };
    let step = |what: &str, r: std::io::Result<()>| match r {
        Ok(()) => log(who, &format!("{what}: ok")),
        Err(e) => log(who, &format!("{what}: FAILED: {e}")),
    };
    step("switch to the free console", ioctl_int(tty0.as_raw_fd(), VT_ACTIVATE, free_vt));
    step("wait until it is shown", ioctl_int(tty0.as_raw_fd(), VT_WAITACTIVE, free_vt));
    step("console to graphics mode", ioctl_int(tty_fd, KDSETMODE, KD_GRAPHICS));
    step("console keyboard off", ioctl_int(tty_fd, KDSKBMODE, K_OFF));
    std::thread::sleep(Duration::from_millis(300));
    let mut master = card.acquire_master_lock();
    for _ in 0..10 {
        if master.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        master = card.acquire_master_lock();
    }
    step("take the display (become its master)", master);

    // Start the part with no rights, with the devices already open.
    keep_across_exec(card.0.as_raw_fd());
    let fds: Vec<String> = inputs
        .iter()
        .map(|f| {
            keep_across_exec(f.as_raw_fd());
            f.as_raw_fd().to_string()
        })
        .collect();
    let exe = std::env::current_exe().expect("own path");
    let mut command = std::process::Command::new(exe);
    command
        .arg("greeter")
        .arg(seconds.to_string())
        .env("TRY_DRM_FD", card.0.as_raw_fd().to_string())
        .env("TRY_INPUT_FDS", fds.join(","))
        .uid(65534)
        .gid(65534);
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            Ok(())
        });
    }
    let started = Instant::now();
    match command.spawn() {
        Ok(mut child) => {
            let deadline = Duration::from_secs(seconds + 8);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        log(who, &format!("the part with no rights ended: {status}"));
                        break;
                    }
                    Ok(None) if started.elapsed() > deadline => {
                        log(who, "the part with no rights did not end in time; stopping it");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(e) => {
                        log(who, &format!("wait failed: {e}"));
                        break;
                    }
                }
            }
        }
        Err(e) => log(who, &format!("RESULT: could not start the part with no rights: {e}")),
    }

    // Give everything back.
    step("give the display up", card.release_master_lock());
    step("console keyboard back", ioctl_int(tty_fd, KDSKBMODE, old_kb));
    step("console to text mode", ioctl_int(tty_fd, KDSETMODE, KD_TEXT));
    step("switch back to the first console", ioctl_int(tty0.as_raw_fd(), VT_ACTIVATE, old_vt));
    step("wait until it is shown", ioctl_int(tty0.as_raw_fd(), VT_WAITACTIVE, old_vt));
    log(who, "done");
}

fn text(pixmap: &mut tiny_skia::Pixmap, font: &fontdue::Font, x: f32, baseline: f32, px: f32, s: &str, rgb: [u8; 3]) {
    let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    let mut pen = x;
    for ch in s.chars() {
        let (m, cover) = font.rasterize(ch, px);
        let left = pen as i32 + m.xmin;
        let top = baseline as i32 - m.height as i32 - m.ymin;
        for row in 0..m.height as i32 {
            for col in 0..m.width as i32 {
                let (px_x, px_y) = (left + col, top + row);
                if px_x < 0 || px_y < 0 || px_x >= w || px_y >= h {
                    continue;
                }
                let a = cover[(row * m.width as i32 + col) as usize] as u32;
                let i = ((px_y * w + px_x) * 4) as usize;
                for c in 0..3 {
                    data[i + c] = ((data[i + c] as u32 * (255 - a) + rgb[c] as u32 * a) / 255) as u8;
                }
            }
        }
        pen += m.advance_width;
    }
}

fn rect(pixmap: &mut tiny_skia::Pixmap, x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3]) {
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(rgb[0], rgb[1], rgb[2], 255);
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        pixmap.fill_rect(r, &paint, tiny_skia::Transform::identity(), None);
    }
}

fn greeter(seconds: u64) {
    let who = "no-rights part";
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    log(who, &format!("uid {uid} euid {euid}"));
    let direct = OpenOptions::new().read(true).write(true).open("/dev/dri/card0");
    log(who, &format!("opening the display device by itself: {}", match direct {
        Ok(_) => "allowed".to_string(),
        Err(e) => format!("refused ({e})"),
    }));
    let drm_fd: RawFd = std::env::var("TRY_DRM_FD").expect("TRY_DRM_FD").parse().unwrap();
    let card = Card(unsafe { File::from_raw_fd(drm_fd) });
    let inputs: Vec<File> = std::env::var("TRY_INPUT_FDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.parse::<RawFd>().ok())
        .map(|fd| unsafe { File::from_raw_fd(fd) })
        .collect();

    let res = match card.resource_handles() {
        Ok(r) => r,
        Err(e) => {
            log(who, &format!("RESULT: cannot read the display's resources: {e}"));
            std::process::exit(4);
        }
    };
    let cons: Vec<connector::Info> = res.connectors().iter().flat_map(|c| card.get_connector(*c, true)).collect();
    log(who, &format!("{} connectors, {} connected", cons.len(), cons.iter().filter(|c| c.state() == connector::State::Connected).count()));
    let Some(con) = cons.iter().find(|c| c.state() == connector::State::Connected) else {
        log(who, "RESULT: no connected screen");
        std::process::exit(5);
    };
    let Some(&mode) = con.modes().first() else {
        log(who, "RESULT: the screen offers no mode");
        std::process::exit(5);
    };
    let (w, h) = mode.size();
    log(who, &format!("mode {}x{} at {} Hz", w, h, mode.vrefresh()));
    let Some(&crtc) = res.crtcs().first() else {
        log(who, "RESULT: no display pipeline");
        std::process::exit(5);
    };
    let mut db = match card.create_dumb_buffer((w.into(), h.into()), DrmFourcc::Xrgb8888, 32) {
        Ok(db) => db,
        Err(e) => {
            log(who, &format!("RESULT: cannot make a screen buffer: {e}"));
            std::process::exit(6);
        }
    };
    let fb = match card.add_framebuffer(&db, 24, 32) {
        Ok(fb) => fb,
        Err(e) => {
            log(who, &format!("RESULT: cannot register the screen buffer: {e}"));
            std::process::exit(6);
        }
    };
    let pitch = db.pitch() as usize;

    // The still part of the picture, drawn once.
    let font = fontdue::Font::from_bytes(FONT_TTF, fontdue::FontSettings::default()).expect("font");
    let t0 = Instant::now();
    let art = tiny_skia::Pixmap::decode_png(FRAME_PNG).expect("frame art");
    let mut base = tiny_skia::Pixmap::new(w as u32, h as u32).expect("pixmap");
    base.fill(tiny_skia::Color::from_rgba8(13, 23, 17, 255));
    let scale = (w as f32 / art.width() as f32).max(h as f32 / art.height() as f32);
    let (dx, dy) = ((w as f32 - art.width() as f32 * scale) / 2.0, (h as f32 - art.height() as f32 * scale) / 2.0);
    let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
    base.draw_pixmap(0, 0, art.as_ref(), &paint, tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy), None);
    let u = h as f32 / 1080.0;
    text(&mut base, &font, 0.26 * w as f32, 0.40 * h as f32, 84.0 * u, "Good evening, Ada.", [242, 238, 228]);
    text(&mut base, &font, 0.26 * w as f32, 0.47 * h as f32, 16.0 * u, "Password", [186, 197, 183]);
    log(who, &format!("still picture drawn in {} ms", t0.elapsed().as_millis()));

    let (mut keys, mut events, mut px, mut py) = (0u32, 0u64, w as f32 / 2.0, h as f32 / 2.0);
    let (mut frames, mut worst, mut total) = (0u32, 0u128, 0u128);
    let mut dirty = true;
    let mut shown = false;
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut pollfds: Vec<libc::pollfd> = inputs.iter().map(|f| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
    let mut buf = [0u8; 24 * 64];
    'run: while Instant::now() < end {
        if dirty {
            let t = Instant::now();
            let mut frame = base.clone();
            let (fx, fy, fw, fh) = (0.26 * w as f32, 0.49 * h as f32, 560.0 * u, 60.0 * u);
            rect(&mut frame, fx, fy, fw, fh, [199, 217, 186]);
            rect(&mut frame, fx + 1.0, fy + 1.0, fw - 2.0, fh - 2.0, [13, 23, 17]);
            for i in 0..keys.min(28) {
                rect(&mut frame, fx + 20.0 * u + i as f32 * 18.0 * u, fy + fh / 2.0 - 4.0 * u, 8.0 * u, 8.0 * u, [242, 238, 228]);
            }
            rect(&mut frame, fx, fy + fh + 30.0 * u, 150.0 * u, 52.0 * u, [178, 47, 39]);
            text(&mut frame, &font, fx + 34.0 * u, fy + fh + 63.0 * u, 16.0 * u, "LOG IN", [242, 238, 228]);
            let line = format!("reclogin try   uid {uid}   keys {keys}   input events {events}   pointer {:.0},{:.0}", px, py);
            text(&mut frame, &font, 0.26 * w as f32, 0.80 * h as f32, 18.0 * u, &line, [154, 168, 157]);
            rect(&mut frame, px - 6.0, py - 6.0, 12.0, 12.0, [240, 184, 102]);
            match card.map_dumb_buffer(&mut db) {
                Ok(mut map) => {
                    let out = map.as_mut();
                    let src = frame.data();
                    for row in 0..h as usize {
                        let line = &mut out[row * pitch..row * pitch + w as usize * 4];
                        let from = &src[row * w as usize * 4..(row + 1) * w as usize * 4];
                        for (o, i) in line.chunks_exact_mut(4).zip(from.chunks_exact(4)) {
                            o[0] = i[2];
                            o[1] = i[1];
                            o[2] = i[0];
                            o[3] = 0;
                        }
                    }
                }
                Err(e) => {
                    log(who, &format!("RESULT: cannot write to the screen buffer: {e}"));
                    std::process::exit(7);
                }
            }
            if !shown {
                match card.set_crtc(crtc, Some(fb), (0, 0), &[con.handle()], Some(mode)) {
                    Ok(()) => log(who, "RESULT: the picture is on the screen (mode set by the part with no rights)"),
                    Err(e) => {
                        log(who, &format!("RESULT: showing the picture FAILED: {e}"));
                        std::process::exit(8);
                    }
                }
                shown = true;
            } else {
                // The same buffer is already shown; tell the driver it changed.
                let _ = card.dirty_framebuffer(fb, &[drm::control::ClipRect::new(0, 0, w, h)]);
            }
            let ms = t.elapsed().as_millis();
            frames += 1;
            total += ms;
            worst = worst.max(ms);
            dirty = false;
        }
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, 40) };
        if n <= 0 {
            continue;
        }
        for (i, p) in pollfds.iter().enumerate() {
            if p.revents & libc::POLLIN == 0 {
                continue;
            }
            let got = unsafe { libc::read(inputs[i].as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if got <= 0 {
                continue;
            }
            for ev in buf[..got as usize].chunks_exact(24) {
                let kind = u16::from_ne_bytes([ev[16], ev[17]]);
                let code = u16::from_ne_bytes([ev[18], ev[19]]);
                let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                events += 1;
                match (kind, code) {
                    (1, 1) if value == 1 => break 'run,
                    (1, c) if value == 1 && c < 256 => {
                        keys += 1;
                        dirty = true;
                    }
                    (2, 0) => {
                        px = (px + value as f32).clamp(0.0, w as f32);
                        dirty = true;
                    }
                    (2, 1) => {
                        py = (py + value as f32).clamp(0.0, h as f32);
                        dirty = true;
                    }
                    (3, 0) => {
                        px = value as f32 / 32767.0 * w as f32;
                        dirty = true;
                    }
                    (3, 1) => {
                        py = value as f32 / 32767.0 * h as f32;
                        dirty = true;
                    }
                    _ => {}
                }
            }
        }
    }
    log(who, &format!("RESULT: {keys} key presses and {events} input events were read through the handed devices"));
    if frames > 0 {
        log(who, &format!("RESULT: {frames} frames, {} ms on average, {worst} ms at worst, drawn on the processor", total / frames as u128));
    }
    let _ = card.destroy_framebuffer(fb);
    let _ = card.destroy_dumb_buffer(db);
    let _ = std::io::stderr().flush();
}

/// `spike session USER TYPE SECONDS COMMAND`: start COMMAND as USER in a new
/// login session of TYPE (wayland or x11) on a free console, registered with
/// the login service the way its PAM module does it, show that console, and
/// take the display back when it ends.
fn session(user: &str, kind: &str, seconds: u64, command: &str) {
    let who = "session";
    let name = std::ffi::CString::new(user).unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        log(who, &format!("RESULT: no user {user}"));
        std::process::exit(2);
    }
    let (uid, gid) = unsafe { ((*pw).pw_uid, (*pw).pw_gid) };
    let home = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) }.to_string_lossy().into_owned();
    let shell = unsafe { std::ffi::CStr::from_ptr((*pw).pw_shell) }.to_string_lossy().into_owned();

    let tty0 = OpenOptions::new().read(true).write(true).open("/dev/tty0").expect("open /dev/tty0");
    let mut state = [0u16; 3];
    unsafe { libc::ioctl(tty0.as_raw_fd(), VT_GETSTATE as _, state.as_mut_ptr()) };
    let old_vt = state[0] as i32;
    let mut vt: i32 = 0;
    if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_OPENQRY as _, &mut vt) } < 0 || vt <= 0 {
        log(who, &format!("no free console: {}", errno()));
        std::process::exit(2);
    }
    log(who, &format!("the display is on console {old_vt}; the new session gets console {vt}"));
    // A login hands the console device to the user, as login(1) and display
    // managers do; an X server without root rights opens it by itself.
    let tty_path = std::ffi::CString::new(format!("/dev/tty{vt}")).unwrap();
    let handed = unsafe { libc::chown(tty_path.as_ptr(), uid, u32::MAX) };
    log(who, &format!("hand /dev/tty{vt} to {user}: {}", if handed == 0 { "ok".to_string() } else { format!("FAILED: {}", errno()) }));

    // The session's first process is made before any thread exists; it waits
    // for the login service's answer.
    let mut pipe = [0i32; 2];
    unsafe { libc::pipe(pipe.as_mut_ptr()) };
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::close(pipe[1]) };
        let mut answer = String::new();
        let mut from = unsafe { File::from_raw_fd(pipe[0]) };
        use std::io::Read;
        let _ = from.read_to_string(&mut answer);
        let mut parts = answer.split('\n');
        let (id, runtime) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if id.is_empty() {
            unsafe { libc::_exit(90) };
        }
        unsafe {
            libc::setsid();
            let tty = std::ffi::CString::new(format!("/dev/tty{vt}")).unwrap();
            let fd = libc::open(tty.as_ptr(), libc::O_RDWR);
            if fd >= 0 {
                libc::ioctl(fd, libc::TIOCSCTTY as _, 0);
                libc::dup2(fd, 0);
            }
            libc::initgroups(name.as_ptr(), gid as _);
            if libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                libc::_exit(91);
            }
        }
        let error = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env_clear()
            .env("HOME", &home)
            .env("USER", user)
            .env("LOGNAME", user)
            .env("SHELL", &shell)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
            .env("LANG", "C.UTF-8")
            .env("XDG_RUNTIME_DIR", runtime)
            .env("XDG_SESSION_ID", id)
            .env("XDG_SEAT", "seat0")
            .env("XDG_VTNR", vt.to_string())
            .env("XDG_SESSION_TYPE", kind)
            .env("XDG_SESSION_CLASS", "user")
            .current_dir(&home)
            .exec();
        eprintln!("[session] exec failed: {error}");
        unsafe { libc::_exit(92) };
    }
    unsafe { libc::close(pipe[0]) };
    let mut to_child = unsafe { File::from_raw_fd(pipe[1]) };

    let created = (|| -> Result<(String, String, zbus::zvariant::OwnedFd, u32, bool), zbus::Error> {
        let bus = zbus::blocking::Connection::system()?;
        let props: Vec<(String, zbus::zvariant::Value)> = Vec::new();
        let reply = bus.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "CreateSession",
            &(uid, child as u32, "reclogin-try", kind, "user", "", "seat0", vt as u32, format!("tty{vt}"), "", false, "", "", props),
        )?;
        let (id, _path, runtime, fifo, _uid, _seat, vtnr, existing): (
            String,
            zbus::zvariant::OwnedObjectPath,
            String,
            zbus::zvariant::OwnedFd,
            u32,
            String,
            u32,
            bool,
        ) = reply.body().deserialize()?;
        Ok((id, runtime, fifo, vtnr, existing))
    })();
    let (id, _fifo) = match created {
        Ok((id, runtime, fifo, vtnr, existing)) => {
            log(who, &format!("the login service made session {id} on console {vtnr} (runtime folder {runtime}; already existed: {existing})"));
            let _ = to_child.write_all(format!("{id}\n{runtime}\n").as_bytes());
            (id, fifo)
        }
        Err(e) => {
            log(who, &format!("RESULT: the login service refused to make a session: {e}"));
            drop(to_child);
            unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) };
            std::process::exit(3);
        }
    };
    drop(to_child);

    let step = |what: &str, r: std::io::Result<()>| match r {
        Ok(()) => log(who, &format!("{what}: ok")),
        Err(e) => log(who, &format!("{what}: FAILED: {e}")),
    };
    step("show the session's console", ioctl_int(tty0.as_raw_fd(), VT_ACTIVATE, vt));
    step("wait until it is shown", ioctl_int(tty0.as_raw_fd(), VT_WAITACTIVE, vt));

    let started = Instant::now();
    let mut status = 0i32;
    let mut ended = false;
    while started.elapsed() < Duration::from_secs(seconds) {
        if unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) } == child {
            ended = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if ended {
        log(who, &format!("the session's command ended by itself after {} s (wait status {status})", started.elapsed().as_secs()));
    } else {
        log(who, &format!("the session ran for {seconds} s; ending it"));
        unsafe { libc::kill(-child, libc::SIGTERM) };
        std::thread::sleep(Duration::from_secs(2));
        unsafe { libc::kill(-child, libc::SIGKILL) };
        unsafe { libc::waitpid(child, &mut status, 0) };
    }
    log(who, &format!("session {id} closed"));
    unsafe { libc::chown(tty_path.as_ptr(), 0, u32::MAX) };
    step("show the first console again", ioctl_int(tty0.as_raw_fd(), VT_ACTIVATE, old_vt));
    step("wait until it is shown", ioctl_int(tty0.as_raw_fd(), VT_WAITACTIVE, old_vt));
    log(who, "done");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    match args.get(1).map(String::as_str) {
        Some("daemon") => daemon(seconds),
        Some("greeter") => greeter(seconds),
        Some("session") => session(
            &args[2],
            &args[3],
            args.get(4).and_then(|s| s.parse().ok()).unwrap_or(15),
            &args[5],
        ),
        _ => eprintln!("usage: spike daemon SECONDS"),
    }
}
