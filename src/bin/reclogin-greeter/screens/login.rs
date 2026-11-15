//! The normal login screen (`docs/reclogin-design/Main.dc.html` and
//! `Acorn.dc.html`). Every position and size below is the design's, in
//! 1920×1080 units; colours and names come from the brand and the state.

use reclogin::protocol::{MessageKind, PowerAction, PromptKind, State};

use super::{Clock, Target};
use crate::form::{Focus, Form, Row};
use crate::paint::icons::{self, Icon};
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

/// The content box inside the frame art.
pub(super) const LEFT: f32 = 250.0;
pub(super) const TOP: f32 = 140.0;
pub(super) const WIDTH: f32 = 1420.0;
pub(super) const HEIGHT: f32 = 800.0;
pub(super) const RIGHT: f32 = LEFT + WIDTH;
pub(super) const BOTTOM: f32 = TOP + HEIGHT;

/// The user list column and the column right of it.
pub(super) const LIST_W: f32 = 420.0;
const COLUMN_GAP: f32 = 110.0;
pub(super) const FORM_X: f32 = LEFT + LIST_W + COLUMN_GAP;
pub(super) const FORM_W: f32 = WIDTH - LIST_W - COLUMN_GAP;

pub(super) const ROW_H: f32 = 68.0;
pub(super) const FIELD_W: f32 = 560.0;
pub(super) const FIELD_H: f32 = 60.0;
pub(super) const FOOTER_H: f32 = 1.0 + 22.0 + 44.0;

pub struct View<'a> {
    pub state: &'a State,
    pub form: &'a Form,
    pub clock: &'a Clock,
}

pub fn draw(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);

    // --- the measures the vertical layout depends on --------------------
    let header_h = header_height(p);
    let list_h = list_height(p, v);

    let chosen = v.form.chosen(v.state).map(|(_, u)| u);
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

    let lines = form_lines(v, true);
    let heights = line_heights(p, &lines);
    let lines_h: f32 = heights.iter().map(|h| 10.0 + h).sum();
    let password_block_h = p.line_height(&sans(12.0, pal.text)) + 10.0 + FIELD_H + lines_h;
    let form_h = greeting.height + 34.0 + password_block_h + 34.0 + 52.0;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - FOOTER_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    header(p, v, header_h);

    // --- who is logging in ----------------------------------------------
    draw_list(p, v, grid_top);

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

    let y = draw_lines(p, &lines, &heights, field_top + FIELD_H);
    let menu = action_row(p, v, y + 34.0, "LOG IN");
    footer(p, v);
    if let Some(menu) = menu {
        session_menu(p, v, menu);
    }
}

/// The right edge of the user list.
const RIGHT_OF_LIST: f32 = LEFT + LIST_W;

pub(super) enum Line {
    Hint(String),
    Notice(MessageKind, String),
    Caps,
}

/// The colour and icon of a message: an error in the error style with the
/// alert sign, reclogind's confirmation with a tick, PAM's information in
/// the secondary text colour.
pub(super) fn message_look(
    p: &Painter,
    kind: MessageKind,
) -> (reclogin::brand::Rgb, Option<&'static Icon>) {
    let pal = p.palette();
    match kind {
        MessageKind::Error => (pal.bad, Some(&icons::ALERT)),
        MessageKind::Success => (pal.ok, Some(&icons::CHECK)),
        MessageKind::Info => (pal.text_secondary, None),
    }
}

/// Whether the field shows a failure (its border in the error colour).
pub(super) fn failed(form: &Form) -> bool {
    matches!(form.notice, Some((MessageKind::Error, _)))
        || form
            .talk
            .messages
            .iter()
            .any(|(kind, _)| *kind == MessageKind::Error)
}

/// The lines under the password field: PAM's messages of the last attempt,
/// the error line and the Caps Lock warning when there are any (as in the
/// wrong-password design), otherwise the layout hint.
pub(super) fn form_lines(v: &View, hint: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    for (kind, text) in &v.form.talk.messages {
        for part in text.lines().filter(|l| !l.trim().is_empty()) {
            lines.push(Line::Notice(*kind, part.to_string()));
        }
    }
    if let Some((kind, text)) = &v.form.notice {
        for part in text.lines().filter(|l| !l.trim().is_empty()) {
            lines.push(Line::Notice(*kind, part.to_string()));
        }
    }
    if v.form.caps_lock {
        lines.push(Line::Caps);
    }
    if hint && lines.is_empty() {
        lines.push(Line::Hint(format!(
            "Typed with the {} layout.",
            layout_label(v.state)
        )));
    }
    lines
}

pub(super) fn layout_label(state: &State) -> String {
    state
        .layouts
        .iter()
        .find(|l| l.name == state.current_layout)
        .map(|l| l.label.clone())
        .unwrap_or_else(|| state.current_layout.clone())
}

pub(super) fn header_height(p: &Painter) -> f32 {
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

pub(super) fn header(p: &mut Painter, v: &View, _height: f32) {
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

/// "LOGGED IN" with its dot at the right end of a row; the width it takes.
fn logged_in_mark(p: &mut Painter, center: f32) -> f32 {
    let pal = p.palette();
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
    label.width + 8.0 + 7.0 + 18.0
}

pub(super) fn user_row(
    p: &mut Painter,
    top: f32,
    index: Option<usize>,
    real_name: &str,
    logged_in: bool,
    chosen: bool,
) {
    let pal = p.palette();
    let center = top + (ROW_H - 1.0) / 2.0;
    number_and_avatar_outline(p, center, index, chosen);
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
        room -= logged_in_mark(p, center);
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

/// The field being typed into, its top at `top`: the login password, or
/// the answer to PAM's question (a visible question shows its text plainly
/// and has no show-password button).
pub(super) fn password_field(p: &mut Painter, v: &View, top: f32) {
    let pal = p.palette();
    let focused = v.form.focus == Focus::Password;
    let failed = failed(v.form);
    let visible = matches!(v.form.talk.asking, Some((_, PromptKind::Visible)));
    p.fill_rect(FORM_X, top, FIELD_W, FIELD_H, pal.surface_sunken);
    let (border_w, border_c) = if failed {
        (2.0, pal.bad)
    } else if focused {
        (1.0, pal.accent)
    } else {
        (1.0, pal.rule)
    };
    p.border(FORM_X, top, FIELD_W, FIELD_H, border_w, border_c);

    let text_room = if visible { FIELD_W } else { FIELD_W - FIELD_H };
    let style = Style::new(Face::Sans, 22.0, pal.text).tracking(0.2);
    let count = v.form.password.chars().count();
    let laid = if v.form.show_password || visible {
        // The person asked to see it (or PAM's question shows what is
        // typed); this copy is not overwritten after use.
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
    if visible {
        return;
    }

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

pub(super) fn notice_line(
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

pub(super) fn footer(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let top = BOTTOM - FOOTER_H;
    p.rule_h(LEFT, top, WIDTH, 1.0, pal.rule);
    let row_top = top + 1.0 + 22.0;
    let center = row_top + 22.0;
    let style = Style::new(Face::Sans, 14.0, pal.text_secondary);

    let mut x = LEFT;
    let w = layout_switcher(p, v, style, x, center);
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
        // The action the question is about is marked (the design's
        // "Shut down" lit and underlined in the accent).
        let marked = v.form.question == Some(action);
        let own = if marked {
            style.with_color(pal.text)
        } else {
            style
        };
        footer_button(p, icon, label, own, x, center);
        if marked {
            p.rule_h(x, row_top + 43.0, w, 1.0, pal.accent);
        }
        if v.form.focus == Focus::Power(action) {
            p.border(x - 8.0, row_top, w + 16.0, 44.0, 1.0, pal.accent);
        }
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

/// The height of the "Who is logging in" label and the list under it.
pub(super) fn list_height(p: &Painter, v: &View) -> f32 {
    let pal = p.palette();
    let label_h = p.line_height(&Style::new(Face::Sans, 11.0, pal.text_muted).tracking(0.14));
    let rows = v.state.users.len() + usize::from(v.state.guest_offered) + 1;
    label_h + 18.0 + 1.0 + rows as f32 * ROW_H
}

/// The label and the list: the users, the guest, "Other user…". Whichever
/// row is chosen is drawn as chosen, and a focused list gets its ring.
pub(super) fn draw_list(p: &mut Painter, v: &View, grid_top: f32) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let label_h = p.line_height(&sans(11.0, pal.text_muted).tracking(0.14));
    p.text(
        "WHO IS LOGGING IN",
        sans(11.0, pal.text_muted).tracking(0.14),
        LEFT,
        grid_top,
    );
    let list_top = grid_top + label_h + 18.0;
    p.rule_h(LEFT, list_top, LIST_W, 1.0, pal.rule);
    let ring = |p: &mut Painter, top: f32| {
        if v.form.focus == Focus::Users {
            p.border(LEFT, top, LIST_W, ROW_H - 1.0, 1.0, pal.accent);
        }
    };
    let mut row_top = list_top + 1.0;
    for (i, user) in v.state.users.iter().enumerate() {
        let is_chosen = v.form.row == Row::User(user.name.clone());
        user_row(
            p,
            row_top,
            Some(i),
            &user.real_name,
            user.logged_in,
            is_chosen,
        );
        if is_chosen {
            ring(p, row_top);
        }
        p.hit(LEFT, row_top, LIST_W, ROW_H, Target::User(i));
        row_top += ROW_H;
    }
    if v.state.guest_offered {
        let chosen = v.form.row == Row::Guest;
        let center = row_top + (ROW_H - 1.0) / 2.0;
        number_and_avatar_outline(p, center, None, chosen);
        if chosen {
            p.icon(
                &icons::USER_BOLD,
                LEFT + 40.0 + 10.0,
                center - 10.0,
                20.0,
                pal.accent_ink,
            );
        } else {
            p.icon(
                &icons::USER,
                LEFT + 40.0 + 10.0,
                center - 10.0,
                20.0,
                pal.text_secondary,
            );
        }
        name_text_at(
            p,
            "Guest",
            center,
            if chosen { pal.text } else { pal.text_secondary },
            LIST_W,
        );
        if crate::form::guest_running(v.state) {
            logged_in_mark(p, center);
        } else if !chosen {
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
        }
        p.rule_h(LEFT, row_top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
        if chosen {
            ring(p, row_top);
        }
        p.hit(LEFT, row_top, LIST_W, ROW_H, Target::Guest);
        row_top += ROW_H;
    }
    {
        let chosen = v.form.row == Row::Other;
        let center = row_top + (ROW_H - 1.0) / 2.0;
        if chosen {
            p.fill_rect(LEFT + 40.0, center - 20.0, 40.0, 40.0, pal.accent);
            p.icon(
                &icons::PLUS_BOLD,
                LEFT + 40.0 + 11.0,
                center - 9.0,
                18.0,
                pal.accent_ink,
            );
        } else {
            p.dashed_border(LEFT + 40.0, center - 20.0, 40.0, 40.0, pal.rule);
            p.icon(
                &icons::PLUS,
                LEFT + 40.0 + 11.0,
                center - 9.0,
                18.0,
                pal.text_muted,
            );
        }
        let other = p.lay(
            &[(
                "Other user…",
                sans(16.0, if chosen { pal.text } else { pal.text_secondary }),
            )],
            None,
            false,
        );
        p.put(&other, LEFT + 98.0, center - other.height / 2.0);
        p.rule_h(LEFT, row_top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
        if chosen {
            ring(p, row_top);
        }
        p.hit(LEFT, row_top, LIST_W, ROW_H, Target::OtherUser);
    }
}

/// Where the session menu opens: its left edge and the top of the row of
/// buttons it belongs to.
pub(super) struct Menu {
    x: f32,
    row_top: f32,
}

/// The main button, and beside it the session picker (only when there is a
/// choice of sessions). `top` is the row's top; the row is 52 high.
pub(super) fn action_row(p: &mut Painter, v: &View, top: f32, label: &str) -> Option<Menu> {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let button = p.lay(
        &[(label, sans(13.0, pal.on_action).weight(600).tracking(0.1))],
        None,
        false,
    );
    let button_w = 34.0 + button.width + 34.0;
    p.fill_rect(FORM_X, top, button_w, 52.0, pal.action);
    p.put(&button, FORM_X + 34.0, top + (52.0 - button.height) / 2.0);
    if v.form.focus == Focus::LogIn {
        p.border(FORM_X - 4.0, top - 4.0, button_w + 8.0, 60.0, 1.0, pal.text);
    }
    p.hit(FORM_X, top, button_w, 52.0, Target::LogIn);

    if !v.form.picker(v.state) {
        return None;
    }
    let session = v
        .form
        .session
        .as_ref()
        .and_then(|id| v.state.sessions.iter().find(|s| &s.id == id))?;
    let open = v.form.menu.is_some();
    let x = FORM_X + button_w + 28.0;
    let center = top + 26.0;
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
        if open { pal.accent } else { pal.rule },
    );
    let chevron_x = name_x + name.width + 10.0;
    let chevron = if open {
        &icons::CHEVRON_UP
    } else {
        &icons::CHEVRON_DOWN
    };
    p.icon(chevron, chevron_x, center - 7.0, 14.0, pal.text_secondary);
    let w = chevron_x + 14.0 - x;
    if v.form.focus == Focus::Session {
        p.border(x - 8.0, top, w + 16.0, 52.0, 1.0, pal.accent);
    }
    p.hit(x, top, w, 52.0, Target::Session);
    Some(Menu {
        x: x + w + 20.0,
        row_top: top,
    })
}

/// The open session menu, drawn over everything else
/// (`AcornOtherUser.dc.html`): "Installed desktops", then each session; the
/// one in use is ticked, the highlighted one has the lighter ground.
pub(super) fn session_menu(p: &mut Painter, v: &View, menu: Menu) {
    let Some(highlight) = v.form.menu else {
        return;
    };
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    const W: f32 = 300.0;
    const OPTION_H: f32 = 50.0;
    let head_style = sans(11.0, pal.text_muted).tracking(0.14);
    let head_text_h = p.line_height(&head_style);
    let head_h = 12.0 + head_text_h + 10.0 + 1.0;
    let h = 1.0 + head_h + v.state.sessions.len() as f32 * OPTION_H + 1.0;
    let mut top = menu.row_top - 34.0;
    if top + h > BOTTOM {
        top = (BOTTOM - h).max(TOP);
    }
    let x = menu.x;
    p.fill_rect(x, top, W, h, pal.surface_raised);
    p.border(x, top, W, h, 1.0, pal.rule);
    p.hit(x, top, W, h, Target::Menu);
    p.text(
        "INSTALLED DESKTOPS",
        head_style,
        x + 1.0 + 18.0,
        top + 1.0 + 12.0,
    );
    p.rule_h(x + 1.0, top + 1.0 + head_h - 1.0, W - 2.0, 1.0, pal.rule);
    let lit = mix(pal.surface_raised, pal.accent, 0.065);
    let mut y = top + 1.0 + head_h;
    for (i, session) in v.state.sessions.iter().enumerate() {
        let current = v.form.session.as_deref() == Some(session.id.as_str());
        let lifted = i == highlight;
        if lifted {
            p.fill_rect(x + 1.0, y, W - 2.0, OPTION_H, lit);
        }
        let color = if current || lifted {
            pal.text
        } else {
            pal.text_secondary
        };
        let name = p.lay(
            &[(&session.name, sans(16.0, color))],
            Some(W - 2.0 - 36.0 - 30.0),
            true,
        );
        p.put(&name, x + 1.0 + 18.0, y + (OPTION_H - name.height) / 2.0);
        if current {
            p.icon(
                &icons::CHECK,
                x + W - 1.0 - 18.0 - 18.0,
                y + (OPTION_H - 18.0) / 2.0,
                18.0,
                pal.accent,
            );
        }
        p.hit(x + 1.0, y, W - 2.0, OPTION_H, Target::SessionOption(i));
        y += OPTION_H;
    }
}

/// `a` moved `t` of the way to `b`.
fn mix(a: reclogin::brand::Rgb, b: reclogin::brand::Rgb, t: f32) -> reclogin::brand::Rgb {
    let part = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    reclogin::brand::Rgb(part(a.0, b.0), part(a.1, b.1), part(a.2, b.2))
}

/// The keyboard icon and the configured layouts: with one it is a plain
/// label; with several they are all shown, the one in use lit and
/// underlined, and a click or Enter switches to the next. Returns the width.
fn layout_switcher(p: &mut Painter, v: &View, style: Style, x: f32, center: f32) -> f32 {
    let pal = p.palette();
    if v.state.layouts.len() < 2 {
        return footer_button(
            p,
            &icons::KEYBOARD,
            &layout_label(v.state),
            style,
            x,
            center,
        );
    }
    p.icon(&icons::KEYBOARD, x, center - 10.0, 20.0, style.color);
    let mut at = x + 30.0;
    for (i, layout) in v.state.layouts.iter().enumerate() {
        let current = layout.name == v.state.current_layout;
        let color = if current { pal.text } else { pal.text_muted };
        let laid = p.lay(&[(&layout.label, style.with_color(color))], None, false);
        if current {
            let top = center - (laid.height + 3.0) / 2.0;
            p.put(&laid, at, top);
            p.rule_h(at, top + laid.height + 2.0, laid.width, 1.0, pal.accent);
        } else {
            p.put(&laid, at, center - laid.height / 2.0);
        }
        at += laid.width;
        if i + 1 < v.state.layouts.len() {
            at += 10.0;
        }
    }
    at - x
}

/// The height of each line under a field.
pub(super) fn line_heights(p: &Painter, lines: &[Line]) -> Vec<f32> {
    let pal = p.palette();
    lines
        .iter()
        .map(|line| match line {
            Line::Hint(_) => p.line_height(&Style::new(Face::Sans, 13.0, pal.text_muted)),
            _ => p
                .line_height(&Style::new(Face::Sans, 15.0, pal.text))
                .max(18.0),
        })
        .collect()
}

/// Draw the lines under a field, each 10 below the one before, starting at
/// `y`; returns where the last one ends.
pub(super) fn draw_lines(p: &mut Painter, lines: &[Line], heights: &[f32], mut y: f32) -> f32 {
    let pal = p.palette();
    for (line, &h) in lines.iter().zip(heights) {
        y += 10.0;
        match line {
            Line::Hint(text) => {
                p.text(
                    text,
                    Style::new(Face::Sans, 13.0, pal.text_muted),
                    FORM_X,
                    y,
                );
            }
            Line::Notice(kind, text) => {
                let (color, icon) = message_look(p, *kind);
                notice_line(p, text, color, icon, y, h);
            }
            Line::Caps => notice_line(p, "Caps Lock is on.", pal.warn, Some(&icons::CAPS), y, h),
        }
        y += h;
    }
    y
}
