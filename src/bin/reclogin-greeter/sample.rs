//! `--render-png`: one frame of the login screen to a PNG file, for
//! development. Opens no device and no socket.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use reclogin::brand::Brand;
use reclogin::protocol::{Layout, Power, State, User};
use tiny_skia::Pixmap;

use crate::form::Form;
use crate::paint::Look;
use crate::screens::{login, Clock};

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

fn read_state(path: &Path) -> Result<State> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

pub fn render(options: &Options) -> Result<()> {
    let brand = Brand::load(&options.brand_dir)?;
    let mut look = Look::load(brand)?;
    let state = match &options.state {
        Some(path) => read_state(path)?,
        None => sample_state(),
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
    let mut form = Form::new(&state);
    for _ in 0..options.typed {
        form.password.push('x');
    }
    let clock = Clock::at(now, state.clock_24h);
    let (w, h) = options.size;
    let mut pixmap = Pixmap::new(w, h).context("frame size")?;
    let mut painter = look.begin(&mut pixmap);
    login::draw(
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
