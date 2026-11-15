//! "Other user…" (`docs/reclogin-design/AcornOtherUser.dc.html`) and the
//! screen for a system with no listed account
//! (`docs/reclogin-design/LevitateNoUsers.dc.html`): a user name field and
//! the password field. The first has the list with its "Other user…" row
//! chosen; the second has a note in place of the list. Positions and sizes
//! are the designs', in 1920×1080 units.

use super::login::{
    action_row, draw_lines, draw_list, footer, form_lines, header, header_height, line_heights,
    list_height, password_field, session_menu, View, FIELD_H, FIELD_W, FOOTER_H, FORM_W, FORM_X,
    HEIGHT, LEFT, LIST_W, TOP,
};
use super::{Screen, Target};
use crate::form::Focus;
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

pub fn draw(p: &mut Painter, v: &View) {
    let pal = p.palette();
    let sans = |size: f32, color| Style::new(Face::Sans, size, color);
    let no_users = v.form.screen(v.state) == Screen::NoUsers;

    // --- measures ---------------------------------------------------------
    let header_h = header_height(p);
    let caps_label = sans(11.0, pal.text_muted).tracking(0.14);
    let label_h = p.line_height(&caps_label);

    let headline_style = Style::new(Face::SerifDisplay, 76.0, pal.text)
        .tracking(-0.035)
        .line_height(0.98);
    let (head, tail) = if no_users {
        ("Log in with ", "a user name.")
    } else {
        ("Log in as ", "someone else.")
    };
    let headline = p.lay(
        &[
            (head, headline_style),
            (tail, headline_style.italic().with_color(pal.accent)),
        ],
        Some(FORM_W),
        false,
    );

    let note = no_users.then(|| Note::lay(p));
    let list_h = match &note {
        Some(note) => label_h + 18.0 + note.height,
        None => list_height(p, v),
    };

    let field_label = sans(12.0, pal.text_secondary);
    let field_label_h = p.line_height(&field_label);
    let lines = form_lines(v, false);
    let heights = line_heights(p, &lines);
    let lines_h: f32 = heights.iter().map(|h| 10.0 + h).sum();
    let fields_h = 2.0 * (field_label_h + 10.0 + FIELD_H) + 20.0 + lines_h;
    let form_h = headline.height + 30.0 + fields_h + 30.0 + 52.0;

    let grid_h = list_h.max(form_h);
    let gap = ((HEIGHT - header_h - grid_h - FOOTER_H) / 2.0).max(0.0);
    let grid_top = TOP + header_h + gap;

    header(p, v, header_h);

    // --- the list, or the note in its place ---------------------------------
    match &note {
        Some(note) => {
            p.text("WHO IS LOGGING IN", caps_label, LEFT, grid_top);
            note.draw(p, grid_top + label_h + 18.0);
        }
        None => draw_list(p, v, grid_top),
    }

    // --- headline, fields, buttons ------------------------------------------
    p.put(&headline, FORM_X, grid_top);
    let mut y = grid_top + headline.height + 30.0;
    p.text("User name", field_label, FORM_X, y);
    y += field_label_h + 10.0;
    user_name_field(p, v, y);
    y += FIELD_H + 20.0;
    p.text("Password", field_label, FORM_X, y);
    y += field_label_h + 10.0;
    password_field(p, v, y);
    y = draw_lines(p, &lines, &heights, y + FIELD_H);

    let menu = action_row(p, v, y + 30.0, "LOG IN");
    footer(p, v);
    if let Some(menu) = menu {
        session_menu(p, v, menu);
    }
}

/// The user name field: plain text, 20 units, the focus ring in the accent.
pub(super) fn user_name_field(p: &mut Painter, v: &View, top: f32) {
    let pal = p.palette();
    let focused = v.form.focus == Focus::UserName;
    p.fill_rect(FORM_X, top, FIELD_W, FIELD_H, pal.surface_sunken);
    p.border(
        FORM_X,
        top,
        FIELD_W,
        FIELD_H,
        1.0,
        if focused { pal.accent } else { pal.rule },
    );
    let style = Style::new(Face::Sans, 20.0, pal.text);
    let text_x = FORM_X + 20.0;
    let laid = p.lay(&[(v.form.user_name.as_str(), style)], None, false);
    let shift = (laid.width - (FIELD_W - 40.0)).max(0.0);
    p.put_clipped(
        &laid,
        text_x - shift,
        top + (FIELD_H - laid.height) / 2.0,
        (FORM_X + 1.0, top + 1.0, FIELD_W - 2.0, FIELD_H - 2.0),
    );
    if focused {
        let empty = v.form.user_name.is_empty();
        let caret_x = text_x - shift + if empty { 0.0 } else { laid.width };
        p.fill_rect(caret_x, top + (FIELD_H - 28.0) / 2.0, 2.0, 28.0, pal.text);
    }
    p.hit(FORM_X, top, FIELD_W, FIELD_H, Target::UserName);
}

/// "No account on this computer is listed here." and what to do about it.
struct Note {
    title: crate::paint::Laid,
    body: crate::paint::Laid,
    keys: Vec<crate::paint::Laid>,
    after: crate::paint::Laid,
    key_h: f32,
    height: f32,
}

const NOTE_PAD: f32 = 24.0;
const NOTE_GAP: f32 = 14.0;

impl Note {
    fn lay(p: &mut Painter) -> Note {
        let pal = p.palette();
        let sans = |size: f32, color| Style::new(Face::Sans, size, color);
        let title = p.lay(
            &[(
                "No account on this computer is listed here.",
                Style::new(Face::Serif, 27.0, pal.text)
                    .tracking(-0.01)
                    .line_height(1.15),
            )],
            Some(LIST_W),
            false,
        );
        let body = p.lay(
            &[(
                "Accounts are made during installation. An administrator can add one from a text console.",
                sans(16.0, pal.text_secondary).line_height(1.55),
            )],
            Some(LIST_W),
            false,
        );
        let keys: Vec<_> = ["Ctrl", "Alt", "F2"]
            .iter()
            .map(|k| p.lay(&[(k, sans(13.0, pal.text_secondary))], None, false))
            .collect();
        let after = p.lay(&[("opens one.", sans(16.0, pal.text_muted))], None, false);
        let key_h = keys.iter().map(|k| k.height).fold(0.0, f32::max) + 8.0 + 2.0;
        let row_h = key_h.max(after.height);
        let height = 1.0
            + NOTE_PAD
            + title.height
            + NOTE_GAP
            + body.height
            + NOTE_GAP
            + row_h
            + NOTE_PAD
            + 1.0;
        Note {
            title,
            body,
            keys,
            after,
            key_h,
            height,
        }
    }

    /// Draw it with its top rule at `top`.
    fn draw(&self, p: &mut Painter, top: f32) {
        let pal = p.palette();
        p.rule_h(LEFT, top, LIST_W, 1.0, pal.rule);
        let mut y = top + 1.0 + NOTE_PAD;
        p.put(&self.title, LEFT, y);
        y += self.title.height + NOTE_GAP;
        p.put(&self.body, LEFT, y);
        y += self.body.height + NOTE_GAP;
        let row_h = self.key_h.max(self.after.height);
        let center = y + row_h / 2.0;
        let mut x = LEFT;
        for key in &self.keys {
            let w = key.width + 16.0 + 2.0;
            p.border(x, center - self.key_h / 2.0, w, self.key_h, 1.0, pal.rule);
            p.put(key, x + 1.0 + 8.0, center - key.height / 2.0);
            x += w + 10.0;
        }
        p.put(&self.after, x, center - self.after.height / 2.0);
        p.rule_h(LEFT, top + self.height - 1.0, LIST_W, 1.0, pal.rule);
    }
}
