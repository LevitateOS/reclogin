//! The two screens that have no field: the guest
//! (`docs/reclogin-design/AcornGuest.dc.html`) and returning to a person's
//! running session (`docs/reclogin-design/LevitateReturn.dc.html`). A
//! headline, a few lines saying what the button does, and the button. What
//! reclogind answers (it may not be able to yet) shows under the lines as
//! the error line does on the login screen. Positions and sizes are the
//! designs', in 1920×1080 units.

use super::login::{
    action_row, draw_lines, draw_list, footer, form_lines, header, header_height, line_heights,
    list_height, session_menu, Line, View, FOOTER_H, FORM_W, FORM_X, HEIGHT, TOP,
};
use super::Screen;
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
    let (head, tail, texts, button): (&str, String, Vec<(String, bool)>, String) = match screen {
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
    let texts_h: f32 = laid.iter().map(|l| l.height).sum::<f32>() + 12.0 * (laid.len() - 1) as f32;
    let lines: Vec<Line> = form_lines(v, false)
        .into_iter()
        .filter(|l| !matches!(l, Line::Caps))
        .collect();
    let heights = line_heights(p, &lines);
    let lines_h: f32 = heights.iter().map(|h| 10.0 + h).sum();
    let form_h = headline.height + 30.0 + texts_h + lines_h + 30.0 + 52.0;

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
    let y = draw_lines(p, &lines, &heights, y);

    let menu = action_row(p, v, y + 30.0, &button);
    footer(p, v);
    if let Some(menu) = menu {
        session_menu(p, v, menu);
    }
}
