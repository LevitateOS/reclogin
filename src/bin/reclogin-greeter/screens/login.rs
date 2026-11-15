//! The normal login screen (`docs/reclogin-design/Main.dc.html` and
//! `Acorn.dc.html`). Every position and size below is the design's, in
//! 1920×1080 units; colours and names come from the brand and the state.

use reclogin::protocol::{MessageKind, PowerAction, State};

use super::{Clock, Target};
use crate::form::{Focus, Form};
use crate::paint::icons::{self, Icon};
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

/// The content box inside the frame art.
const LEFT: f32 = 250.0;
const TOP: f32 = 140.0;
const WIDTH: f32 = 1420.0;
const HEIGHT: f32 = 800.0;
const RIGHT: f32 = LEFT + WIDTH;
const BOTTOM: f32 = TOP + HEIGHT;

/// The user list column and the column right of it.
const LIST_W: f32 = 420.0;
const COLUMN_GAP: f32 = 110.0;
const FORM_X: f32 = LEFT + LIST_W + COLUMN_GAP;
const FORM_W: f32 = WIDTH - LIST_W - COLUMN_GAP;

const ROW_H: f32 = 68.0;
const FIELD_W: f32 = 560.0;
const FIELD_H: f32 = 60.0;
const FOOTER_H: f32 = 1.0 + 22.0 + 44.0;

pub struct View<'a> {
    pub state: &'a State,
    pub form: &'a Form,
    pub clock: &'a Clock,
}

pub fn draw(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let caps_label = |color| sans(11.0, color).tracking(0.14);

    // --- the measures the vertical layout depends on --------------------
    let header_h = header_height(p);
    let rows = v.state.users.len() + usize::from(v.state.guest_offered) + 1;
    let label_h = p.line_height(&caps_label(pal.text_muted));
    let list_h = label_h + 18.0 + 1.0 + rows as f32 * ROW_H;

    let chosen = v.form.chosen.and_then(|i| v.state.users.get(i));
    let first_name = chosen.map(|u| {
        u.real_name
            .split_whitespace()
            .next()
            .unwrap_or(&u.name)
            .to_string()
    });
    let greeting_style = Style::new(Face::SerifDisplay, 84.0, pal.text)
        .tracking(-0.035)
        .line_height(0.98);
    let opening = match &first_name {
        Some(_) => format!("{}, ", v.clock.greeting),
        None => format!("{}.", v.clock.greeting),
    };
    let name_text = first_name.as_ref().map(|n| format!("{n}."));
    let mut spans: Vec<(&str, Style)> = vec![(&opening, greeting_style)];
    if let Some(name) = &name_text {
        spans.push((name, greeting_style.italic().with_color(pal.accent)));
    }
    let greeting = p.lay(&spans, Some(FORM_W), false);

    let lines = form_lines(v);
    let heights: Vec<f32> = lines
        .iter()
        .map(|line| match line {
            Line::Hint(_) => p.line_height(&sans(13.0, pal.text_muted)),
            _ => p.line_height(&sans(15.0, pal.text)).max(18.0),
        })
        .collect();
    let lines_h: f32 = heights.iter().map(|h| 10.0 + h).sum();
    let password_block_h = p.line_height(&sans(12.0, pal.text)) + 10.0 + FIELD_H + lines_h;
    let form_h = greeting.height + 34.0 + password_block_h + 34.0 + 52.0;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - FOOTER_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    header(p, v, header_h);

    // --- who is logging in ----------------------------------------------
    p.text(
        "WHO IS LOGGING IN",
        caps_label(pal.text_muted),
        LEFT,
        grid_top,
    );
    let list_top = grid_top + label_h + 18.0;
    p.rule_h(LEFT, list_top, LIST_W, 1.0, pal.rule);
    let mut row_top = list_top + 1.0;
    for (i, user) in v.state.users.iter().enumerate() {
        let is_chosen = v.form.chosen == Some(i);
        user_row(p, row_top, i, &user.real_name, user.logged_in, is_chosen);
        if is_chosen && v.form.focus == Focus::Users {
            p.border(LEFT, row_top, LIST_W, ROW_H - 1.0, 1.0, pal.accent);
        }
        p.hit(LEFT, row_top, LIST_W, ROW_H, Target::User(i));
        row_top += ROW_H;
    }
    if v.state.guest_offered {
        let center = row_top + (ROW_H - 1.0) / 2.0;
        number_and_avatar_outline(p, center, None, false);
        p.icon(
            &icons::USER,
            LEFT + 40.0 + 10.0,
            center - 10.0,
            20.0,
            pal.text_secondary,
        );
        name_text_at(p, "Guest", center, pal.text_secondary, LIST_W);
        let note = p.lay(
            &[("Nothing is kept after logout", sans(13.0, pal.text_muted))],
            None,
            false,
        );
        p.put(
            &note,
            RIGHT_OF_LIST - note.width,
            center - note.height / 2.0,
        );
        p.rule_h(LEFT, row_top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
        p.hit(LEFT, row_top, LIST_W, ROW_H, Target::Guest);
        row_top += ROW_H;
    }
    {
        let center = row_top + (ROW_H - 1.0) / 2.0;
        p.dashed_border(LEFT + 40.0, center - 20.0, 40.0, 40.0, pal.rule);
        p.icon(
            &icons::PLUS,
            LEFT + 40.0 + 11.0,
            center - 9.0,
            18.0,
            pal.text_muted,
        );
        let other = p.lay(
            &[("Other user…", sans(16.0, pal.text_secondary))],
            None,
            false,
        );
        p.put(&other, LEFT + 98.0, center - other.height / 2.0);
        p.rule_h(LEFT, row_top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
        p.hit(LEFT, row_top, LIST_W, ROW_H, Target::OtherUser);
    }

    // --- greeting, password, log in -------------------------------------
    p.put(&greeting, FORM_X, grid_top);
    let label_top = grid_top + greeting.height + 34.0;
    let (_, label_h12) = p.text(
        "Password",
        sans(12.0, pal.text_secondary),
        FORM_X,
        label_top,
    );
    let field_top = label_top + label_h12 + 10.0;
    password_field(p, v, field_top);

    let mut y = field_top + FIELD_H;
    for (line, &h) in lines.iter().zip(&heights) {
        y += 10.0;
        match line {
            Line::Hint(text) => {
                p.text(text, sans(13.0, pal.text_muted), FORM_X, y);
            }
            Line::Notice(kind, text) => {
                let color = match kind {
                    MessageKind::Error => pal.bad,
                    MessageKind::Info => pal.ok,
                };
                notice_line(
                    p,
                    text,
                    color,
                    if *kind == MessageKind::Error {
                        Some(&icons::ALERT)
                    } else {
                        None
                    },
                    y,
                    h,
                );
            }
            Line::Caps => notice_line(p, "Caps Lock is on.", pal.warn, Some(&icons::CAPS), y, h),
        }
        y += h;
    }

    let buttons_top = y + 34.0;
    let log_in = p.lay(
        &[(
            "LOG IN",
            sans(13.0, pal.on_action).weight(600).tracking(0.1),
        )],
        None,
        false,
    );
    let button_w = 34.0 + log_in.width + 34.0;
    p.fill_rect(FORM_X, buttons_top, button_w, 52.0, pal.action);
    p.put(
        &log_in,
        FORM_X + 34.0,
        buttons_top + (52.0 - log_in.height) / 2.0,
    );
    if v.form.focus == Focus::LogIn {
        p.border(
            FORM_X - 4.0,
            buttons_top - 4.0,
            button_w + 8.0,
            60.0,
            1.0,
            pal.text,
        );
    }
    p.hit(FORM_X, buttons_top, button_w, 52.0, Target::LogIn);

    let session = v
        .form
        .session
        .as_ref()
        .and_then(|id| v.state.sessions.iter().find(|s| &s.id == id));
    if let Some(session) = session {
        let x = FORM_X + button_w + 28.0;
        let center = buttons_top + 26.0;
        let label = p.lay(&[("Session", sans(15.0, pal.text_muted))], None, false);
        p.put(&label, x, center - label.height / 2.0);
        let name = p.lay(&[(&session.name, sans(15.0, pal.text))], None, false);
        let name_x = x + label.width + 10.0;
        let name_top = center - (name.height + 3.0) / 2.0;
        p.put(&name, name_x, name_top);
        p.rule_h(
            name_x,
            name_top + name.height + 2.0,
            name.width,
            1.0,
            pal.rule,
        );
        let chevron_x = name_x + name.width + 10.0;
        p.icon(
            &icons::CHEVRON_DOWN,
            chevron_x,
            center - 7.0,
            14.0,
            pal.text_secondary,
        );
        let w = chevron_x + 14.0 - x;
        if v.form.focus == Focus::Session {
            p.border(x - 8.0, buttons_top, w + 16.0, 52.0, 1.0, pal.accent);
        }
        p.hit(x, buttons_top, w, 52.0, Target::Session);
    }

    footer(p, v);
}

/// The right edge of the user list.
const RIGHT_OF_LIST: f32 = LEFT + LIST_W;

enum Line {
    Hint(String),
    Notice(MessageKind, String),
    Caps,
}

/// The lines under the password field: the message and the Caps Lock
/// warning when there are any (as in the wrong-password design), otherwise
/// the layout hint.
fn form_lines(v: &View) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some((kind, text)) = &v.form.notice {
        for part in text.lines().filter(|l| !l.trim().is_empty()) {
            lines.push(Line::Notice(*kind, part.to_string()));
        }
    }
    if v.form.caps_lock {
        lines.push(Line::Caps);
    }
    if lines.is_empty() {
        lines.push(Line::Hint(format!(
            "Typed with the {} layout.",
            layout_label(v.state)
        )));
    }
    lines
}

fn layout_label(state: &State) -> String {
    state
        .layouts
        .iter()
        .find(|l| l.name == state.current_layout)
        .map(|l| l.label.clone())
        .unwrap_or_else(|| state.current_layout.clone())
}

fn header_height(p: &Painter) -> f32 {
    let pal = p.palette();
    let left = p
        .line_height(&Style::new(Face::Serif, 30.0, pal.text))
        .max(34.0);
    let right = 52.0
        + 4.0
        + p.line_height(&Style::new(Face::Sans, 14.0, pal.text))
        + 4.0
        + p.line_height(&Style::new(Face::Sans, 11.0, pal.text));
    left.max(right)
}

fn header(p: &mut Painter, v: &View, _height: f32) {
    let pal = p.palette();
    let name_style = Style::new(Face::Serif, 30.0, pal.text).tracking(-0.02);
    let row_h = p.line_height(&name_style).max(34.0);
    p.mark(LEFT, TOP + (row_h - 34.0) / 2.0, 34.0, pal.accent);
    let os = p.brand().name.clone();
    let name = p.lay(&[(&os, name_style)], None, false);
    p.put(&name, LEFT + 34.0 + 14.0, TOP + (row_h - name.height) / 2.0);

    let clock = p.lay(
        &[(
            &v.clock.time,
            Style::new(Face::SerifDisplay, 52.0, pal.text)
                .tracking(-0.03)
                .line_height(1.0),
        )],
        None,
        false,
    );
    p.put(&clock, RIGHT - clock.width, TOP);
    let mut y = TOP + clock.height + 4.0;
    let date = p.lay(
        &[(
            &v.clock.date,
            Style::new(Face::Sans, 14.0, pal.text_secondary),
        )],
        None,
        false,
    );
    p.put(&date, RIGHT - date.width, y);
    y += date.height + 4.0;
    let host = v.state.hostname.to_uppercase();
    let host = p.lay(
        &[(
            &host,
            Style::new(Face::Sans, 11.0, pal.text_muted).tracking(0.14),
        )],
        None,
        false,
    );
    p.put(&host, RIGHT - host.width, y);
}

/// The number and the 40×40 outlined box of a row centred on `center`.
fn number_and_avatar_outline(p: &mut Painter, center: f32, number: Option<usize>, chosen: bool) {
    let pal = p.palette();
    if let Some(n) = number {
        let style = Style::new(
            Face::Sans,
            12.0,
            if chosen { pal.accent } else { pal.text_muted },
        )
        .tabular();
        let label = format!("{:02}", n + 1);
        let laid = p.lay(&[(&label, style)], None, false);
        p.put(&laid, LEFT, center - laid.height / 2.0);
    }
    if chosen {
        p.fill_rect(LEFT + 40.0, center - 20.0, 40.0, 40.0, pal.accent);
    } else {
        p.border(LEFT + 40.0, center - 20.0, 40.0, 40.0, 1.0, pal.rule);
    }
}

fn name_text_at(p: &mut Painter, name: &str, center: f32, color: reclogin::brand::Rgb, room: f32) {
    let style = Style::new(Face::Serif, 27.0, color).tracking(-0.01);
    let laid = p.lay(&[(name, style)], Some(room), true);
    p.put(&laid, LEFT + 98.0, center - laid.height / 2.0);
}

fn user_row(
    p: &mut Painter,
    top: f32,
    index: usize,
    real_name: &str,
    logged_in: bool,
    chosen: bool,
) {
    let pal = p.palette();
    let center = top + (ROW_H - 1.0) / 2.0;
    number_and_avatar_outline(p, center, Some(index), chosen);
    let initial: String = real_name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    let initial_style = Style::new(
        Face::Serif,
        22.0,
        if chosen {
            pal.accent_ink
        } else {
            pal.text_secondary
        },
    );
    let laid = p.lay(&[(&initial, initial_style)], None, false);
    p.put(
        &laid,
        LEFT + 40.0 + (40.0 - laid.width) / 2.0,
        center - laid.height / 2.0,
    );

    let mut room = LIST_W - 98.0;
    if logged_in {
        let label = p.lay(
            &[(
                "LOGGED IN",
                Style::new(Face::Sans, 11.0, pal.accent).tracking(0.14),
            )],
            None,
            false,
        );
        let label_x = RIGHT_OF_LIST - label.width;
        p.put(&label, label_x, center - label.height / 2.0);
        p.fill_rect(label_x - 8.0 - 7.0, center - 3.5, 7.0, 7.0, pal.accent);
        room -= label.width + 8.0 + 7.0 + 18.0;
    }
    name_text_at(
        p,
        real_name,
        center,
        if chosen { pal.text } else { pal.text_secondary },
        room,
    );
    p.rule_h(LEFT, top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
}

fn password_field(p: &mut Painter, v: &View, top: f32) {
    let pal = p.palette();
    let focused = v.form.focus == Focus::Password;
    let failed = matches!(v.form.notice, Some((MessageKind::Error, _)));
    p.fill_rect(FORM_X, top, FIELD_W, FIELD_H, pal.surface_sunken);
    let (border_w, border_c) = if failed {
        (2.0, pal.bad)
    } else if focused {
        (1.0, pal.accent)
    } else {
        (1.0, pal.rule)
    };
    p.border(FORM_X, top, FIELD_W, FIELD_H, border_w, border_c);

    let text_room = FIELD_W - FIELD_H;
    let style = Style::new(Face::Sans, 22.0, pal.text).tracking(0.2);
    let count = v.form.password.chars().count();
    let laid = if v.form.show_password {
        // The person asked to see it; this copy is not overwritten after use.
        p.lay(&[(v.form.password.as_str(), style)], None, false)
    } else {
        let dots = "•".repeat(count);
        p.lay(&[(&dots, style)], None, false)
    };
    let text_x = FORM_X + 20.0;
    // Keep the end of a long password in view.
    let shift = (laid.width - (text_room - 40.0)).max(0.0);
    let clip = (
        FORM_X + border_w,
        top + border_w,
        text_room - border_w,
        FIELD_H - 2.0 * border_w,
    );
    p.put_clipped(
        &laid,
        text_x - shift,
        top + (FIELD_H - laid.height) / 2.0,
        clip,
    );
    if focused {
        let caret_x = text_x - shift + if count == 0 { 0.0 } else { laid.width };
        p.fill_rect(caret_x, top + (FIELD_H - 28.0) / 2.0, 2.0, 28.0, pal.text);
    }
    p.hit(FORM_X, top, text_room, FIELD_H, Target::Password);

    let button_x = FORM_X + text_room;
    p.rule_v(
        button_x,
        top + border_w,
        FIELD_H - 2.0 * border_w,
        1.0,
        pal.rule,
    );
    let eye: &Icon = if v.form.show_password {
        &icons::EYE_OFF
    } else {
        &icons::EYE
    };
    p.icon(
        eye,
        button_x + (FIELD_H - 22.0) / 2.0,
        top + (FIELD_H - 22.0) / 2.0,
        22.0,
        pal.text_secondary,
    );
    p.hit(button_x, top, FIELD_H, FIELD_H, Target::ShowPassword);
}

fn notice_line(
    p: &mut Painter,
    text: &str,
    color: reclogin::brand::Rgb,
    icon: Option<&Icon>,
    top: f32,
    height: f32,
) {
    let mut x = FORM_X;
    if let Some(icon) = icon {
        p.icon(icon, x, top + (height - 18.0) / 2.0, 18.0, color);
        x += 18.0 + 10.0;
    }
    let laid = p.lay(
        &[(text, Style::new(Face::Sans, 15.0, color))],
        Some(FIELD_W - (x - FORM_X)),
        false,
    );
    p.put(&laid, x, top + (height - laid.height).max(0.0) / 2.0);
}

fn footer(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let top = BOTTOM - FOOTER_H;
    p.rule_h(LEFT, top, WIDTH, 1.0, pal.rule);
    let row_top = top + 1.0 + 22.0;
    let center = row_top + 22.0;
    let style = Style::new(Face::Sans, 14.0, pal.text_secondary);

    let layout = layout_label(v.state);
    let mut x = LEFT;
    let w = footer_button(p, &icons::KEYBOARD, &layout, style, x, center);
    if v.form.focus == Focus::Layout {
        p.border(x - 8.0, row_top, w + 16.0, 44.0, 1.0, pal.accent);
    }
    p.hit(x, row_top, w, 44.0, Target::Layout);
    x += w + 34.0;
    let w = footer_button(p, &icons::HAND, "On-screen keyboard", style, x, center);
    p.hit(x, row_top, w, 44.0, Target::OnScreenKeyboard);

    let mut actions: Vec<(&Icon, &str, PowerAction)> = Vec::new();
    if v.state.power.sleep {
        actions.push((&icons::MOON, "Sleep", PowerAction::Sleep));
    }
    if v.state.power.restart {
        actions.push((&icons::RESTART, "Restart", PowerAction::Restart));
    }
    if v.state.power.shut_down {
        actions.push((&icons::POWER, "Shut down", PowerAction::ShutDown));
    }
    let widths: Vec<f32> = actions
        .iter()
        .map(|(_, label, _)| 30.0 + p.lay(&[(label, style)], None, false).width)
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 34.0 * widths.len().saturating_sub(1) as f32;
    let mut x = RIGHT - total;
    for ((icon, label, action), w) in actions.into_iter().zip(widths) {
        footer_button(p, icon, label, style, x, center);
        p.hit(x, row_top, w, 44.0, Target::Power(action));
        x += w + 34.0;
    }
}

/// An icon and its label; returns the width.
fn footer_button(
    p: &mut Painter,
    icon: &Icon,
    label: &str,
    style: Style,
    x: f32,
    center: f32,
) -> f32 {
    p.icon(icon, x, center - 10.0, 20.0, style.color);
    let laid = p.lay(&[(label, style)], None, false);
    p.put(&laid, x + 30.0, center - laid.height / 2.0);
    30.0 + laid.width
}
