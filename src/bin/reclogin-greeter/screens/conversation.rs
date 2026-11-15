//! The login screen while PAM asks further questions
//! (`docs/reclogin-design/AcornExpiredPassword.dc.html`): an expired
//! password being changed, or anything else an administrator set up. The
//! user list shrinks to the person logging in and "Someone else" (which
//! cancels); the headline is "Choose a new password."; PAM's messages are
//! listed word for word above the fields; each question is a field with
//! PAM's own prompt as its label, the last one being typed; "Change and log
//! in" answers, "Cancel" abandons the attempt. Positions and sizes are the
//! design's, in 1920×1080 units.

use super::login::{
    footer, header, header_height, message_look, notice_line, password_field, user_row, View,
    FIELD_H, FIELD_W, FOOTER_H, FORM_W, FORM_X, HEIGHT, LEFT, LIST_W, ROW_H, TOP,
};
use super::Target;
use crate::form::{Answered, Focus};
use crate::paint::icons;
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

const MESSAGES_W: f32 = 640.0;
const BACK_H: f32 = 48.0;

pub fn draw(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let caps_label = sans(11.0, pal.text_muted).tracking(0.14);

    // --- measures ---------------------------------------------------------
    let header_h = header_height(p);
    let label_h = p.line_height(&caps_label);
    let list_h = label_h + 18.0 + 1.0 + ROW_H + BACK_H;

    let headline_style = Style::new(Face::SerifDisplay, 76.0, pal.text)
        .tracking(-0.035)
        .line_height(0.98);
    let headline = p.lay(
        &[
            ("Choose a ", headline_style),
            (
                "new password.",
                headline_style.italic().with_color(pal.accent),
            ),
        ],
        Some(FORM_W),
        false,
    );

    let message_style = sans(16.0, pal.text_secondary).line_height(1.5);
    let mut messages = Vec::new();
    for (kind, text) in &v.form.talk.messages {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let (color, icon) = message_look(p, *kind);
            let indent = if icon.is_some() { 28.0 } else { 0.0 };
            let laid = p.lay(
                &[(line, message_style.with_color(color))],
                Some(MESSAGES_W - indent),
                false,
            );
            messages.push((laid, color, icon, indent));
        }
    }
    let messages_h = if messages.is_empty() {
        0.0
    } else {
        messages.iter().map(|m| m.0.height).sum::<f32>() + 8.0 * (messages.len() - 1) as f32
    };

    let field_label = sans(12.0, pal.text_secondary);
    let field_label_h = p.line_height(&field_label);
    let fields = &v.form.talk.fields;
    let fields_h = fields.len() as f32 * (field_label_h + 10.0 + FIELD_H)
        + 18.0 * fields.len().saturating_sub(1) as f32;

    let mut notes = Vec::new();
    if let Some((kind, text)) = &v.form.notice {
        notes.push((*kind, text.clone()));
    }
    let line_h = p.line_height(&sans(15.0, pal.text)).max(18.0);
    let notes_h = (notes.len() + usize::from(v.form.caps_lock)) as f32 * (10.0 + line_h);

    let mut form_h = headline.height + 28.0;
    if !messages.is_empty() {
        form_h += messages_h + 28.0;
    }
    form_h += fields_h + notes_h + 28.0 + 52.0;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - FOOTER_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    header(p, v, header_h);

    // --- who is logging in ------------------------------------------------
    p.text("WHO IS LOGGING IN", caps_label, LEFT, grid_top);
    let list_top = grid_top + label_h + 18.0;
    p.rule_h(LEFT, list_top, LIST_W, 1.0, pal.rule);
    let row_top = list_top + 1.0;
    if let Some((i, user)) = v.form.chosen(v.state) {
        user_row(p, row_top, Some(i), &user.real_name, user.logged_in, true);
    } else {
        // A name typed under "Other user…".
        let name = crate::form::clean_user_name(&v.form.user_name);
        user_row(p, row_top, None, &name, false, true);
    }
    let back_top = row_top + ROW_H;
    let back_center = back_top + BACK_H / 2.0;
    p.icon(
        &icons::CHEVRON_LEFT,
        LEFT,
        back_center - 8.0,
        16.0,
        pal.text_secondary,
    );
    let back = p.lay(
        &[("Someone else", sans(15.0, pal.text_secondary))],
        None,
        false,
    );
    p.put(&back, LEFT + 26.0, back_center - back.height / 2.0);
    p.hit(LEFT, back_top, 26.0 + back.width, BACK_H, Target::Cancel);

    // --- headline, messages, fields, buttons --------------------------------
    p.put(&headline, FORM_X, grid_top);
    let mut y = grid_top + headline.height + 28.0;
    if !messages.is_empty() {
        for (i, (laid, color, icon, indent)) in messages.iter().enumerate() {
            if i > 0 {
                y += 8.0;
            }
            if let Some(icon) = icon {
                // Centred on the first line, as the design's flex row does.
                let first = p.line_height(&message_style).min(laid.height);
                p.icon(icon, FORM_X, y + (first - 18.0) / 2.0, 18.0, *color);
            }
            p.put(laid, FORM_X + indent, y);
            y += laid.height;
        }
        y += 28.0;
    }

    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            y += 18.0;
        }
        let label = field.label.trim_end().trim_end_matches(':').trim_end();
        let (_, h) = p.text(label, field_label, FORM_X, y);
        y += h.max(field_label_h) + 10.0;
        match &field.answer {
            None => password_field(p, v, y),
            Some(answer) => answered_field(p, answer, y),
        }
        y += FIELD_H;
    }
    for (kind, text) in &notes {
        y += 10.0;
        let (color, icon) = message_look(p, *kind);
        notice_line(p, text, color, icon, y, line_h);
        y += line_h;
    }
    if v.form.caps_lock {
        y += 10.0;
        notice_line(
            p,
            "Caps Lock is on.",
            pal.warn,
            Some(&icons::CAPS),
            y,
            line_h,
        );
        y += line_h;
    }

    let buttons_top = y + 28.0;
    let label = p.lay(
        &[(
            "CHANGE AND LOG IN",
            sans(13.0, pal.on_action).weight(600).tracking(0.1),
        )],
        None,
        false,
    );
    let button_w = 34.0 + label.width + 34.0;
    p.fill_rect(FORM_X, buttons_top, button_w, 52.0, pal.action);
    p.put(
        &label,
        FORM_X + 34.0,
        buttons_top + (52.0 - label.height) / 2.0,
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

    let cancel_x = FORM_X + button_w + 28.0;
    let cancel = p.lay(&[("Cancel", sans(15.0, pal.text_secondary))], None, false);
    p.put(
        &cancel,
        cancel_x,
        buttons_top + (52.0 - cancel.height) / 2.0,
    );
    if v.form.focus == Focus::Cancel {
        p.border(
            cancel_x - 8.0,
            buttons_top,
            cancel.width + 16.0,
            52.0,
            1.0,
            pal.accent,
        );
    }
    p.hit(cancel_x, buttons_top, cancel.width, 52.0, Target::Cancel);

    footer(p, v);
}

/// A question already answered: its box with what it may still show.
fn answered_field(p: &mut Painter, answer: &Answered, top: f32) {
    let pal = p.palette();
    p.fill_rect(FORM_X, top, FIELD_W, FIELD_H, pal.surface_sunken);
    p.border(FORM_X, top, FIELD_W, FIELD_H, 1.0, pal.rule);
    let style = Style::new(Face::Sans, 22.0, pal.text).tracking(0.2);
    let text = match answer {
        Answered::Hidden(n) => "•".repeat(*n),
        Answered::Shown(text) => text.clone(),
    };
    let laid = p.lay(&[(&text, style)], None, false);
    p.put_clipped(
        &laid,
        FORM_X + 20.0,
        top + (FIELD_H - laid.height) / 2.0,
        (FORM_X + 1.0, top + 1.0, FIELD_W - 2.0, FIELD_H - 2.0),
    );
}
