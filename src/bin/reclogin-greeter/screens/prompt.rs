//! The two screens that have no field: the guest
//! (`docs/reclogin-design/AcornGuest.dc.html`) and returning to a person's
//! running session (`docs/reclogin-design/LevitateReturn.dc.html`). A
//! headline, a few lines saying what the button does, and the button. The
//! return screen also says which session runs and since when ("Session
//! Plasma (Wayland), running since 19:10", from the state's `running`). Why
//! reclogind could not do what the button asks shows under the lines as the
//! error line does on the login screen. Positions and sizes are the
//! designs', in 1920×1080 units.

use super::login::{
    action_row, draw_lines, draw_list, footer, form_lines, header, header_height, line_heights,
    list_height, session_menu, Line, View, FOOTER_H, FORM_W, FORM_X, HEIGHT, TOP,
};
use super::{Screen, Target};
use chrono::{DateTime, Datelike, Local, TimeZone};
use reclogin::protocol::RunningSession;

use crate::form::guest_running;
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

pub fn draw(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let screen = v.form.screen(v.state);

    // What this screen says, by who it is for.
    let first = v
        .form
        .chosen(v.state)
        .map(|(_, u)| {
            u.real_name
                .split_whitespace()
                .next()
                .unwrap_or(&u.name)
                .to_string()
        })
        .unwrap_or_default();
    // The guest screen while a guest session runs: back to it, or end it.
    let guest_back = screen == Screen::Guest && guest_running(v.state);
    let (head, tail, texts, button): (&str, String, Vec<(String, bool)>, String) = match screen {
        Screen::Guest if guest_back => (
            "Back to ",
            "the guest session.".into(),
            vec![
                (
                    "A guest session is already running on this computer.".into(),
                    false,
                ),
                ("Nothing in it is kept after it ends.".into(), true),
            ],
            "RETURN TO THE GUEST SESSION".into(),
        ),
        Screen::Guest => (
            "Log in as ",
            "a guest.".into(),
            vec![
                (
                    "A guest gets an empty desktop and needs no password.".into(),
                    false,
                ),
                (
                    "Nothing is kept after logout. Files, settings and browsing are removed."
                        .into(),
                    true,
                ),
                (
                    "A guest cannot change the system or see other people's files.".into(),
                    false,
                ),
            ],
            "START GUEST SESSION".into(),
        ),
        _ => (
            "Back to ",
            format!("{first}'s desktop."),
            vec![
                (
                    format!("{first} is already logged in on this computer."),
                    false,
                ),
                (
                    format!("{first}'s desktop asks for the password, not this screen."),
                    true,
                ),
            ],
            format!("RETURN TO {}'S DESKTOP", first.to_uppercase()),
        ),
    };

    // --- measures ---------------------------------------------------------
    let header_h = header_height(p);
    let list_h = list_height(p, v);
    let headline_style = Style::new(Face::SerifDisplay, 84.0, pal.text)
        .tracking(-0.035)
        .line_height(0.98);
    let headline = p.lay(
        &[
            (head, headline_style),
            (&tail, headline_style.italic().with_color(pal.accent)),
        ],
        Some(FORM_W),
        false,
    );
    let laid: Vec<_> = texts
        .iter()
        .map(|(text, bright)| {
            let color = if *bright {
                pal.text
            } else {
                pal.text_secondary
            };
            p.lay(
                &[(text, sans(18.0, color).line_height(1.5))],
                Some(620.0),
                false,
            )
        })
        .collect();
    // "Session   Plasma (Wayland), running since 19:10", on the return screen.
    let session_line = (screen == Screen::Return || guest_back)
        .then(|| {
            if guest_back {
                running_text(v, "guest")
            } else {
                v.form
                    .chosen(v.state)
                    .and_then(|(_, u)| running_text(v, &u.name))
            }
        })
        .flatten()
        .map(|text| {
            (
                p.lay(&[("Session", sans(15.0, pal.text_muted))], None, false),
                p.lay(&[(&text, sans(15.0, pal.text_secondary))], None, false),
            )
        });
    let texts_h: f32 = laid.iter().map(|l| l.height).sum::<f32>()
        + 12.0 * (laid.len() - 1) as f32
        + session_line
            .as_ref()
            .map_or(0.0, |(label, value)| 12.0 + label.height.max(value.height));
    let lines: Vec<Line> = form_lines(v, false)
        .into_iter()
        .filter(|l| !matches!(l, Line::Caps))
        .collect();
    let heights = line_heights(p, &lines);
    let lines_h: f32 = heights.iter().map(|h| 10.0 + h).sum();
    // The quiet second button of a running guest session, under the first.
    let end_guest = guest_back.then(|| {
        p.lay(
            &[("End the guest session", sans(15.0, pal.text_secondary))],
            None,
            false,
        )
    });
    let end_h = end_guest.as_ref().map_or(0.0, |l| 24.0 + l.height + 6.0);
    let form_h = headline.height + 30.0 + texts_h + lines_h + 30.0 + 52.0 + end_h;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - FOOTER_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    header(p, v, header_h);
    draw_list(p, v, grid_top);

    // --- headline, lines, button ----------------------------------------------
    p.put(&headline, FORM_X, grid_top);
    let mut y = grid_top + headline.height + 30.0;
    for (i, text) in laid.iter().enumerate() {
        if i > 0 {
            y += 12.0;
        }
        p.put(text, FORM_X, y);
        y += text.height;
    }
    if let Some((label, value)) = &session_line {
        y += 12.0;
        let h = label.height.max(value.height);
        p.put(label, FORM_X, y + (h - label.height) / 2.0);
        p.put(
            value,
            FORM_X + label.width + 10.0,
            y + (h - value.height) / 2.0,
        );
        y += h;
    }
    let y = draw_lines(p, &lines, &heights, y);

    let menu = action_row(p, v, y + 30.0, &button);
    if let Some(label) = &end_guest {
        let top = y + 30.0 + 52.0 + 24.0;
        p.put(label, FORM_X, top);
        p.rule_h(FORM_X, top + label.height + 2.0, label.width, 1.0, pal.rule);
        p.hit(
            FORM_X,
            top - 8.0,
            label.width,
            label.height + 16.0,
            Target::EndGuest,
        );
    }
    footer(p, v);
    if let Some(menu) = menu {
        session_menu(p, v, menu);
    }
}

/// What the return screen says of a user's session: the desktop and since
/// when. The newest session of `user` when there are several. `None` when
/// the login service told nothing about it.
fn running_text(v: &View, user: &str) -> Option<String> {
    let session: &RunningSession = v
        .state
        .running
        .iter()
        .filter(|s| s.user == user)
        .max_by_key(|s| s.since.unwrap_or(0))?;
    let since = session
        .since
        .and_then(|t| Local.timestamp_opt(t, 0).single())
        .map(|at| since_text(&v.clock.now, &at, v.state.clock_24h));
    match (session.name.trim(), since) {
        ("", None) => None,
        ("", Some(since)) => Some(format!("Running since {since}")),
        (name, None) => Some(name.to_string()),
        (name, Some(since)) => Some(format!("{name}, running since {since}")),
    }
}

/// "19:10" for today, "yesterday 19:10", else "13 Nov 19:10".
fn since_text(now: &DateTime<Local>, at: &DateTime<Local>, hour_24: bool) -> String {
    let time = if hour_24 {
        at.format("%H:%M").to_string()
    } else {
        at.format("%-I:%M %p").to_string()
    };
    let today = now.date_naive();
    let day = at.date_naive();
    if day == today {
        time
    } else if today.pred_opt() == Some(day) {
        format!("yesterday {time}")
    } else {
        format!("{} {} {time}", day.day(), at.format("%b"))
    }
}
