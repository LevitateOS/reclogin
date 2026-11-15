//! What every screen but the primary one shows
//! (`docs/reclogin-design/LevitateSecondScreen.dc.html`): the background art
//! with the mark, the OS name and one line, centred in the content box.
//! Drawn once for a screen and again only when the brand or the size
//! changes; it has no controls and is never hit.

use super::login::{HEIGHT, LEFT, TOP, WIDTH};
use crate::paint::text::{Face, Style};
use crate::paint::Painter;

pub fn draw(p: &mut Painter) {
    let pal = p.palette();
    let os = p.brand().name.clone();
    let name = p.lay(
        &[(
            &os,
            Style::new(Face::Serif, 64.0, pal.text)
                .tracking(-0.03)
                .line_height(1.0),
        )],
        None,
        false,
    );
    let line = p.lay(
        &[(
            "Log in on the other screen.",
            Style::new(Face::Sans, 16.0, pal.text_muted),
        )],
        None,
        false,
    );
    let total = 92.0 + 26.0 + name.height + 26.0 + line.height;
    let center = LEFT + WIDTH / 2.0;
    let mut y = TOP + (HEIGHT - total) / 2.0;
    p.mark(center - 46.0, y, 92.0, pal.accent);
    y += 92.0 + 26.0;
    p.put(&name, center - name.width / 2.0, y);
    y += name.height + 26.0;
    p.put(&line, center - line.width / 2.0, y);
}
