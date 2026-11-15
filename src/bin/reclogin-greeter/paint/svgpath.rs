//! SVG path data (`d`) to a tiny-skia path. Covers every command (M L H V C
//! S Q T A Z, absolute and relative); arcs become cubic curves.

use std::f32::consts::PI;

use tiny_skia::{Path, PathBuilder};

pub fn parse(d: &str) -> Option<Path> {
    let mut p = Parser {
        s: d.as_bytes(),
        i: 0,
    };
    let mut b = PathBuilder::new();
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    // The last control point, for the smooth commands.
    let mut last_cubic: Option<(f32, f32)> = None;
    let mut last_quad: Option<(f32, f32)> = None;
    let mut command = 0u8;
    loop {
        p.skip_separators();
        if p.i >= p.s.len() {
            break;
        }
        let c = p.s[p.i];
        if c.is_ascii_alphabetic() {
            command = c;
            p.i += 1;
        } else if command == 0 {
            return None;
        }
        let rel = command.is_ascii_lowercase();
        let (ox, oy) = if rel { (x, y) } else { (0.0, 0.0) };
        let mut cubic = None;
        let mut quad = None;
        match command.to_ascii_uppercase() {
            b'M' => {
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                b.move_to(nx, ny);
                (x, y, sx, sy) = (nx, ny, nx, ny);
                // Further pairs after a move are lines.
                command = if rel { b'l' } else { b'L' };
            }
            b'L' => {
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                b.line_to(nx, ny);
                (x, y) = (nx, ny);
            }
            b'H' => {
                x = p.number()? + ox;
                b.line_to(x, y);
            }
            b'V' => {
                y = p.number()? + oy;
                b.line_to(x, y);
            }
            b'C' => {
                let (x1, y1) = (p.number()? + ox, p.number()? + oy);
                let (x2, y2) = (p.number()? + ox, p.number()? + oy);
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                b.cubic_to(x1, y1, x2, y2, nx, ny);
                cubic = Some((x2, y2));
                (x, y) = (nx, ny);
            }
            b'S' => {
                let (x1, y1) = last_cubic.map_or((x, y), |(cx, cy)| (2.0 * x - cx, 2.0 * y - cy));
                let (x2, y2) = (p.number()? + ox, p.number()? + oy);
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                b.cubic_to(x1, y1, x2, y2, nx, ny);
                cubic = Some((x2, y2));
                (x, y) = (nx, ny);
            }
            b'Q' => {
                let (x1, y1) = (p.number()? + ox, p.number()? + oy);
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                b.quad_to(x1, y1, nx, ny);
                quad = Some((x1, y1));
                (x, y) = (nx, ny);
            }
            b'T' => {
                let (x1, y1) = last_quad.map_or((x, y), |(cx, cy)| (2.0 * x - cx, 2.0 * y - cy));
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                b.quad_to(x1, y1, nx, ny);
                quad = Some((x1, y1));
                (x, y) = (nx, ny);
            }
            b'A' => {
                let (rx, ry, rotation) = (p.number()?, p.number()?, p.number()?);
                let (large, sweep) = (p.flag()?, p.flag()?);
                let (nx, ny) = (p.number()? + ox, p.number()? + oy);
                arc(&mut b, (x, y), rx, ry, rotation, large, sweep, (nx, ny));
                (x, y) = (nx, ny);
            }
            b'Z' => {
                b.close();
                (x, y) = (sx, sy);
            }
            _ => return None,
        }
        last_cubic = cubic;
        last_quad = quad;
    }
    b.finish()
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_separators(&mut self) {
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',')
        {
            self.i += 1;
        }
    }

    fn number(&mut self) -> Option<f32> {
        self.skip_separators();
        let start = self.i;
        let s = self.s;
        if self.i < s.len() && (s[self.i] == b'-' || s[self.i] == b'+') {
            self.i += 1;
        }
        let mut seen_dot = false;
        let mut seen_digit = false;
        while self.i < s.len() {
            match s[self.i] {
                b'0'..=b'9' => seen_digit = true,
                b'.' if !seen_dot => seen_dot = true,
                _ => break,
            }
            self.i += 1;
        }
        if self.i < s.len() && seen_digit && (s[self.i] == b'e' || s[self.i] == b'E') {
            let save = self.i;
            self.i += 1;
            if self.i < s.len() && (s[self.i] == b'-' || s[self.i] == b'+') {
                self.i += 1;
            }
            let digits = self.i;
            while self.i < s.len() && s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == digits {
                self.i = save;
            }
        }
        if !seen_digit {
            self.i = start;
            return None;
        }
        std::str::from_utf8(&s[start..self.i]).ok()?.parse().ok()
    }

    /// An arc flag: a single 0 or 1, which may run into the next number.
    fn flag(&mut self) -> Option<bool> {
        self.skip_separators();
        let c = *self.s.get(self.i)?;
        self.i += 1;
        match c {
            b'0' => Some(false),
            b'1' => Some(true),
            _ => None,
        }
    }
}

/// An elliptical arc as cubic curves (SVG implementation notes, F.6).
#[allow(clippy::too_many_arguments)]
fn arc(
    b: &mut PathBuilder,
    from: (f32, f32),
    rx: f32,
    ry: f32,
    rotation: f32,
    large: bool,
    sweep: bool,
    to: (f32, f32),
) {
    let (x1, y1) = from;
    let (x2, y2) = to;
    if (x1 - x2).abs() < f32::EPSILON && (y1 - y2).abs() < f32::EPSILON {
        return;
    }
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 {
        b.line_to(x2, y2);
        return;
    }
    let phi = rotation.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let mut coef = if den == 0.0 {
        0.0
    } else {
        (num / den).max(0.0).sqrt()
    };
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos_phi * cxp - sin_phi * cyp + (x1 + x2) / 2.0;
    let cy = sin_phi * cxp + cos_phi * cyp + (y1 + y2) / 2.0;

    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| {
        let dot = ux * vx + uy * vy;
        let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            -a
        } else {
            a
        }
    };
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let theta1 = angle(1.0, 0.0, ux, uy);
    let mut delta = angle(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= 2.0 * PI;
    } else if sweep && delta < 0.0 {
        delta += 2.0 * PI;
    }

    let segments = (delta.abs() / (PI / 2.0)).ceil().max(1.0) as usize;
    let step = delta / segments as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let point = |t: f32| {
        let (s, c) = t.sin_cos();
        (
            cx + rx * c * cos_phi - ry * s * sin_phi,
            cy + rx * c * sin_phi + ry * s * cos_phi,
        )
    };
    let derivative = |t: f32| {
        let (s, c) = t.sin_cos();
        (
            -rx * s * cos_phi - ry * c * sin_phi,
            -rx * s * sin_phi + ry * c * cos_phi,
        )
    };
    let mut t = theta1;
    for _ in 0..segments {
        let t2 = t + step;
        let (p1x, p1y) = point(t);
        let (d1x, d1y) = derivative(t);
        let (p2x, p2y) = point(t2);
        let (d2x, d2y) = derivative(t2);
        b.cubic_to(
            p1x + k * d1x,
            p1y + k * d1y,
            p2x - k * d2x,
            p2y - k * d2y,
            p2x,
            p2y,
        );
        t = t2;
    }
}
