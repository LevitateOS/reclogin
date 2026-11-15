//! `--render-png`: one frame of the login screen to a PNG file, for
//! development. Opens no device and no socket.
//!
//! A `--state` file is a [`State`] in JSON. It may also carry a `"sample"`
//! object (ignored by everything else) that puts the form in a state PAM
//! would leave it in: a failed attempt (`notice`, `caps_lock`), or further
//! questions (`messages`, `fields`), or on another screen: `row` (`"guest"`,
//! `"other"` or a user name), `user_name`, `focus`, `menu` (the session menu
//! open), `question` (the power question, `restart` or `shut_down`) and
//! `running_since` (when the running sessions started, local time). See
//! `assets/dev/state-*.json`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use reclogin::brand::Brand;
use reclogin::protocol::{Layout, MessageKind, Power, PowerAction, PromptKind, State, User};
use serde::Deserialize;
use tiny_skia::Pixmap;

use crate::form::{Answered, Field, Focus, Form, Row};
use crate::paint::Look;
use crate::screens::{self, login, Clock};

/// The optional `"sample"` object of a `--state` file.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Overlay {
    sample: Option<Sample>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Sample {
    /// Characters in the field being typed; overrides `--typed`.
    typed: Option<usize>,
    notice: Option<(MessageKind, String)>,
    caps_lock: bool,
    messages: Vec<(MessageKind, String)>,
    fields: Vec<SampleField>,
    /// The chosen row: `guest`, `other`, or a user name.
    row: Option<String>,
    /// The typed user name.
    user_name: String,
    /// `users`, `user_name`, `password`, `session`, `layout`, `log_in`.
    focus: Option<String>,
    /// The session menu is open.
    menu: bool,
    /// The power question is open for this action: `restart` or `shut_down`.
    question: Option<String>,
    /// Every running session started at this local time, `YYYY-MM-DD HH:MM`
    /// (the file's `since` would depend on the machine's time zone).
    running_since: Option<String>,
}

#[derive(Deserialize)]
struct SampleField {
    label: String,
    kind: PromptKind,
    /// Length of a secret answer already sent; none: being typed.
    #[serde(default)]
    answered: Option<usize>,
}

pub struct Options {
    pub png: PathBuf,
    pub brand_dir: PathBuf,
    pub size: (u32, u32),
    pub state: Option<PathBuf>,
    /// The time shown, `YYYY-MM-DD HH:MM`; now when not given.
    pub at: Option<String>,
    /// How many characters the password field holds.
    pub typed: usize,
}

/// A state with no OS in it, for when no `--state` file is given.
fn sample_state() -> State {
    State {
        hostname: "workstation".into(),
        users: vec![
            User {
                name: "ada".into(),
                real_name: "Ada Lindqvist".into(),
                logged_in: false,
            },
            User {
                name: "kai".into(),
                real_name: "Kai Moreno".into(),
                logged_in: true,
            },
            User {
                name: "noor".into(),
                real_name: "Noor Haddad".into(),
                logged_in: false,
            },
        ],
        sessions: Vec::new(),
        last_session_for: Default::default(),
        last_user: None,
        layouts: vec![Layout {
            name: "us".into(),
            label: "English (US)".into(),
            xkb_layout: "us".into(),
            xkb_variant: String::new(),
            xkb_model: String::new(),
            xkb_options: String::new(),
        }],
        current_layout: "us".into(),
        power: Power {
            sleep: true,
            restart: true,
            shut_down: true,
        },
        guest_offered: true,
        others_logged_in: vec!["kai".into()],
        running: Vec::new(),
        clock_24h: true,
        idle_seconds: None,
    }
}

pub fn parse_size(text: &str) -> Result<(u32, u32)> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .context("--size is WIDTHxHEIGHT")?;
    let (w, h): (u32, u32) = (
        w.parse().context("--size width")?,
        h.parse().context("--size height")?,
    );
    if !(320..=8192).contains(&w) || !(200..=8192).contains(&h) {
        bail!("--size {w}x{h} is out of range");
    }
    Ok((w, h))
}

fn read_state(path: &Path) -> Result<(State, Option<Sample>)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let state =
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    let overlay: Overlay =
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    Ok((state, overlay.sample))
}

pub fn render(options: &Options) -> Result<()> {
    let brand = Brand::load(&options.brand_dir)?;
    let mut look = Look::load(brand)?;
    let (mut state, sample) = match &options.state {
        Some(path) => read_state(path)?,
        None => (sample_state(), None),
    };
    let now = match &options.at {
        Some(text) => {
            let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M")
                .context("--at is YYYY-MM-DD HH:MM")?;
            Local
                .from_local_datetime(&naive)
                .earliest()
                .context("--at is not a local time")?
        }
        None => Local::now(),
    };
    if let Some(text) = sample.as_ref().and_then(|s| s.running_since.as_ref()) {
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M")
            .context("the sample's running_since is YYYY-MM-DD HH:MM")?;
        let at = Local
            .from_local_datetime(&naive)
            .earliest()
            .context("the sample's running_since is not a local time")?;
        for running in &mut state.running {
            running.since = Some(at.timestamp());
        }
    }
    let mut form = Form::new(&state);
    let mut typed = options.typed;
    if let Some(sample) = sample {
        typed = sample.typed.unwrap_or(typed);
        if let Some(row) = &sample.row {
            let row = match row.as_str() {
                "guest" => Row::Guest,
                "other" => Row::Other,
                name => Row::User(name.to_string()),
            };
            form.select(row, &state);
            form.entered(&state);
        }
        form.user_name = sample.user_name;
        if sample.menu {
            form.open_menu(&state);
        }
        if let Some(focus) = &sample.focus {
            form.focus = match focus.as_str() {
                "users" => Focus::Users,
                "user_name" => Focus::UserName,
                "password" => Focus::Password,
                "session" => Focus::Session,
                "layout" => Focus::Layout,
                "log_in" => Focus::LogIn,
                "confirm" => Focus::Confirm,
                "cancel" => Focus::Cancel,
                "sleep" => Focus::Power(PowerAction::Sleep),
                "restart" => Focus::Power(PowerAction::Restart),
                "shut_down" => Focus::Power(PowerAction::ShutDown),
                other => bail!("the sample's focus {other:?} is not one of users, user_name, password, session, layout, log_in, confirm, cancel, sleep, restart, shut_down"),
            };
        }
        if let Some(question) = &sample.question {
            let action = match question.as_str() {
                "restart" => PowerAction::Restart,
                "shut_down" => PowerAction::ShutDown,
                other => bail!("the sample's question {other:?} is not restart or shut_down"),
            };
            form.question = Some(action);
        }
        form.notice = sample.notice;
        form.caps_lock = sample.caps_lock;
        form.talk.messages = sample.messages;
        for (i, field) in sample.fields.into_iter().enumerate() {
            if field.answered.is_none() {
                form.talk.asking = Some((i as u64 + 1, field.kind));
            }
            form.talk.fields.push(Field {
                label: field.label,
                answer: field.answered.map(Answered::Hidden),
            });
        }
        form.talk.asked = !form.talk.fields.is_empty();
    }
    for _ in 0..typed {
        form.password.push('x');
    }
    let clock = Clock::at(now, state.clock_24h);
    let (w, h) = options.size;
    let mut pixmap = Pixmap::new(w, h).context("frame size")?;
    let mut painter = look.begin(&mut pixmap);
    screens::draw(
        &mut painter,
        &login::View {
            state: &state,
            form: &form,
            clock: &clock,
        },
    );
    drop(painter);
    pixmap
        .save_png(&options.png)
        .with_context(|| format!("writing {}", options.png.display()))?;
    Ok(())
}
