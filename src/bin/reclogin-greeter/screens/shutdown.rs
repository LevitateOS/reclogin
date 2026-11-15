//! The question before a restart or shut down while others are logged in
//! (`docs/reclogin-design/AcornShutDown.dc.html`): "Shut down for
//! everyone?", who is still logged in and what it costs them, "… anyway" and
//! "Cancel", with the footer action it is about marked. The list shows
//! everyone, the logged-in ones lit. Positions and sizes are the design's,
//! in 1920×1080 units.

use reclogin::protocol::{MessageKind, PowerAction, State};

use super::login::{
    footer, header, header_height, message_look, notice_line, View, FOOTER_H, FORM_W, FORM_X,
    HEIGHT, LEFT, LIST_W, ROW_H, TOP,
};
use super::Target;
use crate::form::Focus;
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

pub fn draw(p: &mut Painter, v: &View, action: PowerAction) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);

    let (verb, noun, confirm) = match action {
        PowerAction::Restart => ("Restart ", "Restarting", "RESTART ANYWAY"),
        _ => ("Shut down ", "Shutting down", "SHUT DOWN ANYWAY"),
    };

    // --- measures ---------------------------------------------------------
    let header_h = header_height(p);
    let caps_label = sans(11.0, pal.text_muted).tracking(0.14);
    let label_h = p.line_height(&caps_label);
    let list_h = label_h + 18.0 + 1.0 + v.state.users.len() as f32 * ROW_H;

    let headline_style = Style::new(Face::SerifDisplay, 84.0, pal.text)
        .tracking(-0.035)
        .line_height(0.98);
    let headline = p.lay(
        &[
            (verb, headline_style),
            (
                "for everyone?",
                headline_style.italic().with_color(pal.accent),
            ),
        ],
        Some(FORM_W),
        false,
    );

    let who = format!("{} still logged in.", who_text(v.state));
    let cost = format!("{noun} closes their desktops. Work they have not saved is lost.");
    let lines: Vec<_> = [(who, pal.text), (cost, pal.text_secondary)]
        .into_iter()
        .map(|(text, color)| {
            p.lay(
                &[(&text, sans(18.0, color).line_height(1.5))],
                Some(620.0),
                false,
            )
        })
        .collect();
    let texts_h: f32 = lines.iter().map(|l| l.height).sum::<f32>() + 12.0;

    let button_style = sans(13.0, pal.on_action).weight(600).tracking(0.1);
    let confirm_laid = p.lay(&[(confirm, button_style)], None, false);
    let cancel_laid = p.lay(
        &[("CANCEL", button_style.with_color(pal.text))],
        None,
        false,
    );
    let confirm_w = 34.0 + confirm_laid.width + 34.0;
    let cancel_w = 1.0 + 28.0 + cancel_laid.width + 28.0 + 1.0;

    let notice = v
        .form
        .notice
        .as_ref()
        .map(|(kind, text)| (*kind, text.clone()));
    let line_h = p.line_height(&sans(15.0, pal.text)).max(18.0);
    let notice_h = if notice.is_some() { 16.0 + line_h } else { 0.0 };
    let form_h = headline.height + 30.0 + texts_h + 30.0 + 52.0 + notice_h;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - FOOTER_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    header(p, v, header_h);

    // --- everyone, the logged-in ones lit -------------------------------------
    p.text("WHO IS LOGGING IN", caps_label, LEFT, grid_top);
    let list_top = grid_top + label_h + 18.0;
    p.rule_h(LEFT, list_top, LIST_W, 1.0, pal.rule);
    let mut row_top = list_top + 1.0;
    for (i, user) in v.state.users.iter().enumerate() {
        row(p, row_top, i, &user.real_name, user.logged_in);
        row_top += ROW_H;
    }

    // --- the question -------------------------------------------------------------
    p.put(&headline, FORM_X, grid_top);
    let mut y = grid_top + headline.height + 30.0;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            y += 12.0;
        }
        p.put(line, FORM_X, y);
        y += line.height;
    }
    y += 30.0;

    p.fill_rect(FORM_X, y, confirm_w, 52.0, pal.action);
    p.put(
        &confirm_laid,
        FORM_X + 34.0,
        y + (52.0 - confirm_laid.height) / 2.0,
    );
    if v.form.focus == Focus::Confirm {
        p.border(FORM_X - 4.0, y - 4.0, confirm_w + 8.0, 60.0, 1.0, pal.text);
    }
    p.hit(FORM_X, y, confirm_w, 52.0, Target::Confirm);

    let cancel_x = FORM_X + confirm_w + 28.0;
    p.border(cancel_x, y, cancel_w, 52.0, 1.0, pal.accent);
    p.put(
        &cancel_laid,
        cancel_x + 1.0 + 28.0,
        y + (52.0 - cancel_laid.height) / 2.0,
    );
    if v.form.focus == Focus::Cancel {
        p.border(cancel_x - 4.0, y - 4.0, cancel_w + 8.0, 60.0, 1.0, pal.text);
    }
    p.hit(cancel_x, y, cancel_w, 52.0, Target::Cancel);

    if let Some((kind, text)) = notice {
        let (color, icon) = message_look(p, kind);
        let color = if kind == MessageKind::Info {
            pal.text_secondary
        } else {
            color
        };
        notice_line(p, &text, color, icon, y + 52.0 + 16.0, line_h);
    }

    footer(p, v);
}

/// "Kai Moreno and Noor Haddad are": the people with a session, by the
/// name shown in the list (the account name for one that is not listed),
/// and the verb.
fn who_text(state: &State) -> String {
    let names: Vec<String> = state
        .others_logged_in
        .iter()
        .map(|account| {
            state
                .users
                .iter()
                .find(|u| &u.name == account)
                .map_or_else(|| account.clone(), |u| u.real_name.clone())
        })
        .collect();
    match names.as_slice() {
        [] => "Someone is".into(),
        [one] => format!("{one} is"),
        [init @ .., last] => format!("{} and {last} are", init.join(", ")),
    }
}

/// One person of the list, 68 high: the number, the 40×40 box with the
/// initial, the name, and for a logged-in one the accent box border, the
/// brighter text and "LOGGED IN" with its square on the right.
fn row(p: &mut Painter, top: f32, index: usize, real_name: &str, logged_in: bool) {
    let pal = p.palette();
    let center = top + (ROW_H - 1.0) / 2.0;
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);

    let number = format!("{:02}", index + 1);
    let laid = p.lay(
        &[(&number, sans(12.0, pal.text_muted).tabular())],
        None,
        false,
    );
    p.put(&laid, LEFT, center - laid.height / 2.0);

    let (outline, initial_color, name_color) = if logged_in {
        (pal.accent, pal.text, pal.text)
    } else {
        (pal.rule, pal.text_secondary, pal.text_secondary)
    };
    p.border(LEFT + 40.0, center - 20.0, 40.0, 40.0, 1.0, outline);
    let initial: String = real_name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    let laid = p.lay(
        &[(&initial, Style::new(Face::Serif, 22.0, initial_color))],
        None,
        false,
    );
    p.put(
        &laid,
        LEFT + 40.0 + (40.0 - laid.width) / 2.0,
        center - laid.height / 2.0,
    );

    let mut room = LIST_W - 98.0;
    if logged_in {
        let label = p.lay(
            &[("LOGGED IN", sans(11.0, pal.accent).tracking(0.14))],
            None,
            false,
        );
        let label_x = LEFT + LIST_W - label.width;
        p.put(&label, label_x, center - label.height / 2.0);
        p.fill_rect(label_x - 8.0 - 7.0, center - 3.5, 7.0, 7.0, pal.accent);
        room -= label.width + 8.0 + 7.0 + 18.0;
    }
    let name = p.lay(
        &[(
            real_name,
            Style::new(Face::Serif, 27.0, name_color).tracking(-0.01),
        )],
        Some(room),
        true,
    );
    p.put(&name, LEFT + 98.0, center - name.height / 2.0);
    p.rule_h(LEFT, top + ROW_H - 1.0, LIST_W, 1.0, pal.rule);
}
