// login-screen: reclogin, the login screen of LevitateOS and AcornOS, as a
// Recipe group installed into the system root (`recipe --system install
// login-screen`) with a receipt, so it can be removed or replaced like any
// other group. Removed, the machine starts to its text logins.
//
// NOT ACTIVE YET. This file is the recipe prepared for the switch from SDDM;
// it lives in tools/reclogin/dist/recipe/ until a login-screen prebuilt is
// published for both systems. SWITCH.md beside it says how it replaces
// recipes/login-screen.rhai. It names no packages (no SEEDS, no generated pin
// blocks), so `cargo xtask base groups` leaves it alone, as it leaves rec-apps.
//
// The programs and their files are the OS builder's pinned prebuilt tree (the
// LevitateOS/prebuilt release `login-screen-<key16>` that `prebuilt publish
// login-screen <distro>` makes from tools/reclogin), compiled in each OS's own
// apps-build root against the libpam, libinput and libxkbcommon that OS
// ships: one archive per C library. The pins below follow the newest
// published key; when reclogin or the libraries it is built against change, a
// new key is published and these pins must be moved to it (by hand: nothing
// generates them).
//
// The libraries it links: libpam is in the base on both systems; libinput,
// libxkbcommon (with the keymaps of xkeyboard-config) come with desktop-libs.
const RECIPE_API = 0;
const NEEDS = #{
    outputs: #{ system: "directory" },
    network: ["github.com:443", "release-assets.githubusercontent.com:443", "objects.githubusercontent.com:443"],
    // Two stripped programs, the fonts and the background: set to the
    // archive's size (rounded up) when the pins are filled.
    network_max_bytes: 16777216,
    timeout_secs: 1800,
};

const ABOUT = #{
    kind: "system",
    category: "utilities",
    libc: ["glibc", "musl"],
    systems: ["levitateos", "acornos"],
    details: "reclogin, the login screen: it lists the people who use this computer, asks for the password through the system's own login rules, and starts the desktop they choose. It draws straight to the screen, on console 7; the text logins on consoles 1 to 6 stay. Removing it leaves the text logins.",
    homepage: "https://github.com/LevitateOS/reclogin",
    license: "(MIT OR Apache-2.0) AND OFL-1.1",
    steps: #{
        acquire: "Downloads the pinned prebuilt archive for this system's C library and checks its SHA-256.",
        build: "Unpacks it and keeps the two programs, the login rules (PAM), the settings file, the service, the accounts, the brand files and the guest session's files.",
        install: "Publishes the system tree for Recipe to write into the system root, with a receipt, and starts the login screen at boot.",
    },
    keeps: [],
};

let deps = ["desktop-libs"];

let ctx = #{
    name: "login-screen",
    version: "2026.11.15",
    description: "reclogin, the login screen",
};

// [file, URL, SHA-256] per C library: the release asset and the digest GitHub
// publishes for it. Empty until the first `prebuilt publish login-screen
// levitate` (glibc) and `... acorn` (musl); a system whose C library has no
// entry gets the "no build" message below.
//   glibc: ["login-screen-levitate.tar.gz",
//           "https://github.com/LevitateOS/prebuilt/releases/download/login-screen-<key16>/login-screen-levitate.tar.gz",
//           "<sha256>"],
//   musl:  ["login-screen-acorn.tar.gz",
//           "https://github.com/LevitateOS/prebuilt/releases/download/login-screen-<key16>/login-screen-acorn.tar.gz",
//           "<sha256>"],
const PINS = #{};

// The account the greeter runs as, and the guest account (fixed uid and gid
// 60600, locked, home in memory at /run/reclogin/guest: reclogind uses it only
// when it is exactly this). The tree's usr/lib/sysusers.d/reclogin.conf makes
// them too (Recipe reads a change's own sysusers.d); a line made twice is
// made once. Same lines as tools/reclogin/dist/sysusers/reclogin.conf.
const ACCOUNTS = [
    "u reclogin - 'reclogin login screen' - -",
    "g guest 60600",
    "u guest 60600:60600 'Guest' /run/reclogin/guest /bin/bash",
    "u reclogin-sweep 60601 'reclogin guest sweep' - -",
];

// What the system tree takes from the prebuilt tree: [path, mode], the
// builder's list (distro/builder/os-recipes/reclogin.rhai, installed_files)
// for this system's init. The display-manager link (systemd) and the runlevel
// link (OpenRC) are made in configure.
const BRAND = "usr/share/reclogin/";
const FILES = [
    ["usr/bin/reclogind", 0o755],
    ["usr/bin/reclogin-greeter", 0o755],
    ["etc/pam.d/reclogin", 0o644],
    ["etc/pam.d/reclogin-autologin", 0o644],
    ["etc/reclogin.conf", 0o644],
    ["usr/lib/sysusers.d/reclogin.conf", 0o644],
    [BRAND + "brand.json", 0o644],
    [BRAND + "background.png", 0o644],
    [BRAND + "fonts/Newsreader.ttf", 0o644],
    [BRAND + "fonts/Newsreader-Italic.ttf", 0o644],
    [BRAND + "fonts/NewsreaderDisplay.ttf", 0o644],
    [BRAND + "fonts/NewsreaderDisplay-Italic.ttf", 0o644],
    [BRAND + "fonts/InstrumentSans.ttf", 0o644],
    [BRAND + "fonts/OFL-Newsreader.txt", 0o644],
    [BRAND + "fonts/OFL-InstrumentSans.txt", 0o644],
    // The guest.
    ["etc/pam.d/reclogin-guest", 0o644],
    ["etc/security/limits.d/50-reclogin-guest.conf", 0o644],
    ["etc/ssh/sshd_config.d/50-reclogin-guest.conf", 0o644],
    ["usr/share/polkit-1/rules.d/50-reclogin-guest.rules", 0o644],
    ["usr/libexec/reclogin/reclogin-guest-notice", 0o755],
    [BRAND + "guest-home/.config/autostart/reclogin-guest-notice.desktop", 0o644],
];
// systemd (LevitateOS, glibc) and OpenRC (AcornOS, musl).
const FILES_SYSTEMD = [
    ["usr/lib/systemd/system/reclogin.service", 0o644],
    ["usr/lib/systemd/system/user@60600.service.d/50-reclogin-guest.conf", 0o644],
];
const FILES_OPENRC = [
    ["etc/init.d/reclogin", 0o755],
    ["usr/libexec/reclogin/reclogind-run", 0o755],
];

ctx.libc = RUNTIME.host.libc;
ctx.pin = PINS[ctx.libc];
ctx.init = if ctx.libc == "glibc" { "systemd" } else if ctx.libc == "musl" { "openrc" } else { "" };
ctx.files = FILES + (if ctx.init == "systemd" { FILES_SYSTEMD } else { FILES_OPENRC });
ctx.archive = join_path(RUNTIME.paths.build, "prebuilt.tar.gz");
ctx.unpacked = join_path(RUNTIME.paths.build, "unpacked");
ctx.system = join_path(RUNTIME.paths.output, "system");
ctx.record = join_path(RUNTIME.paths.build, "system.pin");

fn no_build(ctx) {
    "login-screen has no prebuilt reclogin for this system's C library (" + ctx.runtime.host.libc +
        ") yet: it is published per OS by `prebuilt publish login-screen <distro>` and pinned here by hand"
}

fn is_acquired(ctx) {
    if type_of(ctx.pin) == "()" { return needs_work(ctx, no_build(ctx)); }
    if !is_file(ctx.archive) { return needs_work(ctx, "download " + ctx.pin[0]); }
    verify_sha256(ctx.archive, ctx.pin[2]);
    ready(ctx)
}

fn acquire(ctx) {
    if type_of(ctx.pin) == "()" { throw no_build(ctx); }
    download(ctx.pin[1], ctx.archive, ctx.pin[2]);
    ctx
}

fn check_system(ctx) {
    if type_of(ctx.pin) == "()" { return needs_work(ctx, no_build(ctx)); }
    if !is_file(ctx.record) || !is_dir(ctx.system) { return needs_work(ctx, "unpack the login screen"); }
    if read_file(ctx.record) != ctx.pin[2] + "\n" { return needs_work(ctx, "the pinned login screen changed"); }
    ready(ctx)
}

fn is_built(ctx) { check_system(ctx) }

fn build(ctx) {
    if type_of(ctx.pin) == "()" { throw no_build(ctx); }
    verify_sha256(ctx.archive, ctx.pin[2]);
    for dir in [ctx.unpacked, ctx.system] { if exists(dir) { rm(dir); } }
    if exists(ctx.record) { rm(ctx.record); }
    mkdir(ctx.unpacked);
    extract(ctx.archive, ctx.unpacked);
    mkdir(ctx.system);
    for file in ctx.files {
        let from = join_path(ctx.unpacked, file[0]);
        if !is_file(from) { throw "the prebuilt login screen lacks /" + file[0]; }
        let dest = join_path(ctx.system, file[0]);
        mkdir(dirname(dest));
        copy_file(from, dest);
        target_mode(ctx.system, file[0], file[1]);
    }
    rm(ctx.unpacked);
    configure(ctx);
    target_finalize(ctx.system, #{ owners: "apply", accounts: ACCOUNTS });
    write_file(ctx.record, ctx.pin[2] + "\n");
    ctx
}

fn is_installed(ctx) {
    if !is_dir(ctx.system) { return needs_work(ctx, "unpack the login screen"); }
    ctx.outputs.system = ctx.system;
    check_system(ctx)
}

fn install(ctx) {
    ctx.outputs.system = ctx.system;
    ctx
}

fn remove(ctx) {
    for dir in [ctx.system, ctx.record] { if exists(dir) { rm(dir); } }
    ctx.outputs = #{};
    ctx
}

// A symbolic link at `path` to `target`.
fn put_link(ctx, path, target) {
    let dest = join_path(ctx.system, path);
    mkdir(dirname(dest));
    ln_force(target, dest);
}

// Start the login screen at boot, in this system's init's own form. systemd:
// the unit's Alias=, display-manager.service, which graphical.target wants
// (the unit has no WantedBy: nothing else starts it). OpenRC: the service in
// the default runlevel (it needs elogind and dbus, which the base starts).
// Text logins stay on consoles 1 to 6 on both: nothing is masked.
fn configure(ctx) {
    if ctx.init == "systemd" {
        let unit = "usr/lib/systemd/system/reclogin.service";
        if !read_file(join_path(ctx.system, unit)).contains("Alias=display-manager.service") {
            throw "login-screen: /" + unit + " no longer names display-manager.service as its alias";
        }
        put_link(ctx, "etc/systemd/system/display-manager.service", "/" + unit);
    } else if ctx.init == "openrc" {
        if !is_file(join_path(ctx.system, "etc/init.d/reclogin")) { throw "login-screen: the prebuilt has no OpenRC service reclogin"; }
        put_link(ctx, "etc/runlevels/default/reclogin", "/etc/init.d/reclogin");
    } else {
        throw no_build(ctx);
    }
}
