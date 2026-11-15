//! The login screen with the on-screen keyboard open
//! (`docs/reclogin-design/LevitateKeyboard.dc.html`). The user list shrinks
//! to the chosen person and "Someone else", the headline is smaller, the
//! password field and "Log in" share a row, the footer (layout, keyboard
//! button, power) is gone, and the keyboard takes the lower part of the
//! content box: five rows of keys 56 high. Positions and sizes are the
//! design's, in 1920×1080 units.
//!
//! It is drawn for every screen that has a field: the login password, the
//! user name of "Other user…" and a question of PAM's. Not drawn here: the
//! session picker (no room in the design), the power question (that screen
//! hides the keyboard), the guest and return screens (no field).

use reclogin::brand::Rgb;

use super::login::{
    draw_lines, form_lines, layout_label, line_heights, password_field, user_row, View, FIELD_H,
    FIELD_W, FORM_W, FORM_X, HEIGHT, LEFT, LIST_W, RIGHT, ROW_H, TOP, WIDTH,
};
use super::other::user_name_field;
use super::{Screen, Target};
use crate::form::Focus;
use crate::osk::{KeyId, Shift};
use crate::paint::icons;
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

const KEY_H: f32 = 56.0;
const KEY_GAP: f32 = 8.0;
/// The keyboard: its top rule, 18 of space, five rows with gaps between.
const KEYS_H: f32 = 1.0 + 18.0 + 5.0 * KEY_H + 4.0 * KEY_GAP;
const BACK_H: f32 = 48.0;
const BUTTON_H: f32 = 60.0;

pub fn draw(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let caps_label = sans(11.0, pal.text_muted).tracking(0.14);
    let screen = v.form.screen(v.state);
    let talking = v.form.talking();

    // --- measures ---------------------------------------------------------
    let name_style = Style::new(Face::Serif, 30.0, pal.text).tracking(-0.02);
    let clock_style = Style::new(Face::SerifDisplay, 52.0, pal.text)
        .tracking(-0.03)
        .line_height(1.0);
    let date_style = sans(14.0, pal.text_secondary);
    let header_h = p
        .line_height(&name_style)
        .max(34.0)
        .max(p.line_height(&clock_style) + 4.0 + p.line_height(&date_style));
    let label_h = p.line_height(&caps_label);
    let list_h = label_h + 18.0 + 1.0 + ROW_H + BACK_H;

    let headline_style = Style::new(Face::SerifDisplay, 56.0, pal.text)
        .tracking(-0.03)
        .line_height(1.0);
    let first_name = v.form.chosen(v.state).map(|(_, u)| {
        u.real_name
            .split_whitespace()
            .next()
            .unwrap_or(&u.name)
            .to_string()
    });
    let (head, tail): (String, String) = if talking {
        ("Choose a ".into(), "new password.".into())
    } else {
        match (screen, &first_name) {
            (Screen::NoUsers, _) => ("Log in with ".into(), "a user name.".into()),
            (Screen::Other, _) | (_, None) => ("Log in as ".into(), "someone else.".into()),
            (_, Some(name)) => (format!("{}, ", v.clock.greeting), format!("{name}.")),
        }
    };
    let headline = p.lay(
        &[
            (&head, headline_style),
            (&tail, headline_style.italic().with_color(pal.accent)),
        ],
        Some(FORM_W),
        false,
    );

    let field_label = sans(12.0, pal.text_secondary);
    let field_label_h = p.line_height(&field_label);
    let block_h = field_label_h + 10.0 + FIELD_H;

    // PAM's own messages (a question screen only), word for word.
    let message_style = sans(16.0, pal.text_secondary).line_height(1.5);
    let messages: Vec<_> = if talking {
        v.form
            .talk
            .messages
            .iter()
            .flat_map(|(kind, text)| {
                text.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| (*kind, l.to_string()))
                    .collect::<Vec<_>>()
            })
            .collect()
    } else {
        Vec::new()
    };
    let messages_h = if messages.is_empty() {
        0.0
    } else {
        messages.len() as f32 * p.line_height(&message_style)
    };

    // On a question screen PAM's messages are drawn above the field, so the
    // lines under it are only the error and Caps Lock ones.
    let lines: Vec<_> = form_lines(v, false)
        .into_iter()
        .skip(messages.len())
        .collect();
    let heights = line_heights(p, &lines);
    let lines_h: f32 = heights.iter().map(|h| 10.0 + h).sum();

    let body_h = if talking {
        (if messages.is_empty() {
            0.0
        } else {
            messages_h + 22.0
        }) + block_h
    } else if screen.has_user_name() {
        2.0 * block_h + 20.0
    } else {
        block_h
    };
    let form_h = headline.height + 22.0 + body_h + lines_h;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - KEYS_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    // --- header: the mark and the name, the time and the date -------------
    let row_h = p.line_height(&name_style).max(34.0);
    p.mark(LEFT, TOP + (row_h - 34.0) / 2.0, 34.0, pal.accent);
    let os = p.brand().name.clone();
    let name = p.lay(&[(&os, name_style)], None, false);
    p.put(&name, LEFT + 34.0 + 14.0, TOP + (row_h - name.height) / 2.0);
    let clock = p.lay(&[(&v.clock.time, clock_style)], None, false);
    p.put(&clock, RIGHT - clock.width, TOP);
    let date = p.lay(&[(&v.clock.date, date_style)], None, false);
    p.put(&date, RIGHT - date.width, TOP + clock.height + 4.0);

    // --- who is logging in, collapsed ---------------------------------------
    p.text("WHO IS LOGGING IN", caps_label, LEFT, grid_top);
    let list_top = grid_top + label_h + 18.0;
    p.rule_h(LEFT, list_top, LIST_W, 1.0, pal.rule);
    let row_top = list_top + 1.0;
    match v.form.chosen(v.state) {
        Some((i, user)) => user_row(p, row_top, Some(i), &user.real_name, user.logged_in, true),
        None => other_row(p, row_top),
    }
    let back_top = row_top + ROW_H;
    let back_center = back_top + BACK_H / 2.0;
    let (back_text, back_target, show_back) = if talking {
        ("Someone else", Target::Cancel, true)
    } else if v.form.chosen(v.state).is_some() {
        ("Someone else", Target::OtherUser, true)
    } else {
        // "Other user…" is chosen: back to the list, if there is one.
        (
            "Choose from the list",
            Target::User(first_user(v)),
            !v.state.users.is_empty(),
        )
    };
    if show_back {
        p.icon(
            &icons::CHEVRON_LEFT,
            LEFT,
            back_center - 8.0,
            16.0,
            pal.text_secondary,
        );
        let back = p.lay(&[(back_text, sans(15.0, pal.text_secondary))], None, false);
        p.put(&back, LEFT + 26.0, back_center - back.height / 2.0);
        p.hit(LEFT, back_top, 26.0 + back.width, BACK_H, back_target);
    }

    // --- headline, the fields, "Log in" --------------------------------------
    p.put(&headline, FORM_X, grid_top);
    let mut y = grid_top + headline.height + 22.0;
    let field_top;
    if talking {
        if !messages.is_empty() {
            for (kind, text) in &messages {
                let (color, _) = super::login::message_look(p, *kind);
                let laid = p.lay(
                    &[(text, message_style.with_color(color))],
                    Some(FORM_W),
                    false,
                );
                p.put(&laid, FORM_X, y);
                y += laid.height;
            }
            y += 22.0;
        }
        let label = v
            .form
            .talk
            .fields
            .last()
            .map(|f| {
                f.label
                    .trim_end()
                    .trim_end_matches(':')
                    .trim_end()
                    .to_string()
            })
            .unwrap_or_default();
        p.text(&label, field_label, FORM_X, y);
        field_top = y + field_label_h + 10.0;
        password_field(p, v, field_top);
    } else {
        if screen.has_user_name() {
            p.text("User name", field_label, FORM_X, y);
            user_name_field(p, v, y + field_label_h + 10.0);
            y += block_h + 20.0;
        }
        p.text("Password", field_label, FORM_X, y);
        field_top = y + field_label_h + 10.0;
        password_field(p, v, field_top);
    }
    log_in_button(p, v, FORM_X + FIELD_W + 24.0, field_top);
    draw_lines(p, &lines, &heights, field_top + FIELD_H);

    keys(p, v);
}

/// The first user of the list, the row "Choose from the list" returns to:
/// the last one who logged in, else the first.
fn first_user(v: &View) -> usize {
    v.state
        .last_user
        .as_ref()
        .and_then(|name| v.state.users.iter().position(|u| &u.name == name))
        .unwrap_or(0)
}

/// The chosen "Other user…" row: the accent box with the plus.
fn other_row(p: &mut Painter, top: f32) {
    let pal = p.palette();
    let center = top + (ROW_H - 1.0) / 2.0;
    p.fill_rect(LEFT + 40.0, center - 20.0, 40.0, 40.0, pal.accent);
    p.icon(
        &icons::PLUS_BOLD,
        LEFT + 40.0 + 11.0,
        center - 9.0,
        18.0,
        pal.accent_ink,
    );
    let label = p.lay(
        &[("Other user…", Style::new(Face::Sans, 16.0, pal.text))],
        None,
        false,
    );
    p.put(&label, LEFT + 98.0, center - label.height / 2.0);
    p.rule_h(LEFT, top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
}

/// "Log in" (or "Change and log in") beside the password field, 60 high.
fn log_in_button(p: &mut Painter, v: &View, x: f32, top: f32) {
    let pal = p.palette();
    let text = if v.form.talking() {
        "CHANGE AND LOG IN"
    } else {
        "LOG IN"
    };
    let label = p.lay(
        &[(
            text,
            Style::new(Face::Sans, 13.0, pal.on_action)
                .weight(600)
                .tracking(0.1),
        )],
        None,
        false,
    );
    let w = 34.0 + label.width + 34.0;
    p.fill_rect(x, top, w, BUTTON_H, pal.action);
    p.put(&label, x + 34.0, top + (BUTTON_H - label.height) / 2.0);
    if v.form.focus == Focus::LogIn {
        p.border(x - 4.0, top - 4.0, w + 8.0, BUTTON_H + 8.0, 1.0, pal.text);
    }
    p.hit(x, top, w, BUTTON_H, Target::LogIn);
}

/// `a` moved `t` of the way to `b`.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let part = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    Rgb(part(a.0, b.0), part(a.1, b.1), part(a.2, b.2))
}

/// One entry of a row of keys: a width, or the rest of the row.
#[derive(Clone, Copy)]
enum Width {
    Fixed(f32),
    Flex,
}

/// The `(x, width)` of each key of a row spanning `x0..x0 + total`.
fn spread(x0: f32, total: f32, items: &[Width]) -> Vec<(f32, f32)> {
    let gaps = KEY_GAP * items.len().saturating_sub(1) as f32;
    let fixed: f32 = items
        .iter()
        .map(|w| if let Width::Fixed(f) = w { *f } else { 0.0 })
        .sum();
    let flexes = items
        .iter()
        .filter(|w| matches!(w, Width::Flex))
        .count()
        .max(1);
    let flex = (total - gaps - fixed) / flexes as f32;
    let mut x = x0;
    items
        .iter()
        .map(|w| {
            let width = match w {
                Width::Fixed(f) => *f,
                Width::Flex => flex,
            };
            let at = (x, width);
            x += width + KEY_GAP;
            at
        })
        .collect()
}

/// The five rows of keys, with the rule above them.
fn keys(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let top = super::login::TOP + HEIGHT - KEYS_H;
    p.rule_h(LEFT, top, WIDTH, 1.0, pal.rule);
    let mut y = top + 1.0 + 18.0;
    let osk = &v.form.keyboard;
    let letter = |p: &mut Painter, row: u8, i: usize, x: f32, y: f32, w: f32| {
        let id = KeyId::Char(row, i as u8);
        let glyph = osk.glyph(row.into(), i).map(String::from);
        cap(
            p,
            v,
            id,
            (x, y, w),
            Cap::Letter,
            glyph.as_deref().unwrap_or(""),
        );
    };

    // Digits.
    for (i, (x, w)) in spread(LEFT, WIDTH, &[Width::Flex; 10])
        .into_iter()
        .enumerate()
    {
        letter(p, 0, i, x, y, w);
    }
    y += KEY_H + KEY_GAP;
    // Q row, 30 in from each side.
    for (i, (x, w)) in spread(LEFT + 30.0, WIDTH - 60.0, &[Width::Flex; 10])
        .into_iter()
        .enumerate()
    {
        letter(p, 1, i, x, y, w);
    }
    y += KEY_H + KEY_GAP;
    // A row, 60 in from each side, then Enter.
    let mut items = vec![Width::Flex; 9];
    items.push(Width::Fixed(190.0));
    let places = spread(LEFT + 60.0, WIDTH - 120.0, &items);
    for (i, (x, w)) in places.iter().copied().take(9).enumerate() {
        letter(p, 2, i, x, y, w);
    }
    let (x, w) = places[9];
    cap(p, v, KeyId::Enter, (x, y, w), Cap::Enter, "ENTER");
    y += KEY_H + KEY_GAP;
    // Shift, Z row, Delete.
    let mut items = vec![Width::Fixed(150.0)];
    items.extend([Width::Flex; 9]);
    items.push(Width::Fixed(150.0));
    let places = spread(LEFT, WIDTH, &items);
    let shift_label = if osk.shift == Shift::Lock {
        "Shift lock"
    } else {
        "Shift"
    };
    cap(
        p,
        v,
        KeyId::Shift,
        (places[0].0, y, places[0].1),
        Cap::Function,
        shift_label,
    );
    for (i, (x, w)) in places[1..10].iter().copied().enumerate() {
        letter(p, 3, i, x, y, w);
    }
    let (x, w) = places[10];
    cap(p, v, KeyId::Delete, (x, y, w), Cap::Function, "Delete");
    y += KEY_H + KEY_GAP;
    // Symbols, layout, space, hide.
    let places = spread(
        LEFT,
        WIDTH,
        &[
            Width::Fixed(150.0),
            Width::Fixed(190.0),
            Width::Flex,
            Width::Fixed(230.0),
        ],
    );
    let symbols = if osk.symbols { "abc" } else { "!#1" };
    cap(
        p,
        v,
        KeyId::Symbols,
        (places[0].0, y, places[0].1),
        Cap::Function,
        symbols,
    );
    let layout = layout_label(v.state);
    cap(
        p,
        v,
        KeyId::Layout,
        (places[1].0, y, places[1].1),
        Cap::Function,
        &layout,
    );
    cap(
        p,
        v,
        KeyId::Space,
        (places[2].0, y, places[2].1),
        Cap::Letter,
        "",
    );
    cap(
        p,
        v,
        KeyId::Hide,
        (places[3].0, y, places[3].1),
        Cap::Hide,
        "Hide keyboard",
    );
}

/// What kind of key a cap is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cap {
    /// A character key (and Space): lighter ground, 20-unit text.
    Letter,
    /// Shift, Delete, symbols, layout: the darker ground, 15 units.
    Function,
    Enter,
    Hide,
}

/// One key: its ground, border and label, and its hit area.
fn cap(p: &mut Painter, v: &View, id: KeyId, at: (f32, f32, f32), kind: Cap, label: &str) {
    let pal = p.palette();
    let (x, y, w) = at;
    let osk = &v.form.keyboard;
    let pressed = osk.pressed == Some(id);
    let lit = match id {
        KeyId::Shift => osk.shift != Shift::Off,
        KeyId::Symbols => osk.symbols,
        _ => false,
    };
    let locked = id == KeyId::Shift && osk.shift == Shift::Lock;
    let base = match kind {
        Cap::Letter => pal.surface_raised,
        Cap::Function | Cap::Hide => pal.surface,
        Cap::Enter => pal.action,
    };
    let ground = if locked {
        pal.accent
    } else if pressed {
        mix(base, pal.accent, 0.35)
    } else {
        base
    };
    p.fill_rect(x, y, w, KEY_H, ground);
    if kind != Cap::Enter {
        let edge = if pressed || lit { pal.accent } else { pal.rule };
        p.border(x, y, w, KEY_H, 1.0, edge);
    }
    let center = y + KEY_H / 2.0;
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    match kind {
        Cap::Letter => {
            let laid = p.lay(&[(label, sans(20.0, pal.text))], None, false);
            p.put(
                &laid,
                x + (w - laid.width) / 2.0,
                center - laid.height / 2.0,
            );
        }
        Cap::Function => {
            let color = if locked {
                pal.accent_ink
            } else if lit || pressed {
                pal.text
            } else {
                pal.text_secondary
            };
            let laid = p.lay(&[(label, sans(15.0, color))], Some(w - 16.0), true);
            p.put(
                &laid,
                x + (w - laid.width) / 2.0,
                center - laid.height / 2.0,
            );
        }
        Cap::Enter => {
            let laid = p.lay(
                &[(label, sans(13.0, pal.on_action).weight(600).tracking(0.1))],
                None,
                false,
            );
            p.put(
                &laid,
                x + (w - laid.width) / 2.0,
                center - laid.height / 2.0,
            );
        }
        Cap::Hide => {
            let color = if pressed {
                pal.text
            } else {
                pal.text_secondary
            };
            let laid = p.lay(&[(label, sans(15.0, color))], None, false);
            let total = 18.0 + 10.0 + laid.width;
            let left = x + (w - total) / 2.0;
            p.icon(&icons::CHEVRON_DOWN, left, center - 9.0, 18.0, color);
            p.put(&laid, left + 28.0, center - laid.height / 2.0);
        }
    }
    p.hit(x, y, w, KEY_H, Target::Key(id));
}
