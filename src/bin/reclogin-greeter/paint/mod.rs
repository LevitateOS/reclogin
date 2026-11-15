//! Drawing on the processor: the brand's look loaded once, and a painter
//! that draws in design units (1920×1080) scaled as a whole to the screen.

pub mod icons;
pub mod svgpath;
pub mod text;

use anyhow::{Context, Result};
use reclogin::brand::{Brand, Palette, Rgb};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, PixmapPaint,
    Rect, Stroke, StrokeDash, Transform,
};

use crate::screens::Target;
use icons::{Icon, Shape};
use text::{Block, Style, Text};

/// The design's size; everything is laid out in these units.
pub const DESIGN_W: f32 = 1920.0;
pub const DESIGN_H: f32 = 1080.0;

/// The brand, loaded: palette, mark, background art and fonts.
pub struct Look {
    pub brand: Brand,
    art: Pixmap,
    mark: Vec<(Path, Option<f32>, FillRule)>,
    pub text: Text,
    /// The background at the last size drawn.
    background: Option<Pixmap>,
}

impl Look {
    pub fn load(brand: Brand) -> Result<Look> {
        let art_path = brand.background_path();
        let bytes =
            std::fs::read(&art_path).with_context(|| format!("reading {}", art_path.display()))?;
        let art = Pixmap::decode_png(&bytes)
            .with_context(|| format!("decoding {} (PNG)", art_path.display()))?;
        let mut mark = Vec::new();
        for (i, path) in brand.mark.paths.iter().enumerate() {
            let parsed = svgpath::parse(&path.d)
                .with_context(|| format!("the mark's path {} is not SVG path data", i + 1))?;
            let rule = if path.even_odd {
                FillRule::EvenOdd
            } else {
                FillRule::Winding
            };
            if path.fill {
                mark.push((parsed.clone(), None, rule));
            }
            if let Some(width) = path.stroke_width {
                mark.push((parsed, Some(width), rule));
            }
        }
        let text = Text::load(&brand)?;
        Ok(Look {
            brand,
            art,
            mark,
            text,
            background: None,
        })
    }

    /// Start a frame of the size of `target`: the background art covering
    /// the whole screen, and a painter for the design area, scaled as a
    /// whole and centred.
    pub fn begin<'a>(&'a mut self, target: &'a mut Pixmap) -> Painter<'a> {
        let (w, h) = (target.width(), target.height());
        let cached = self
            .background
            .as_ref()
            .is_some_and(|b| b.width() == w && b.height() == h);
        if !cached {
            self.background = Some(cover(&self.art, w, h, self.brand.palette.surface_sunken));
        }
        if let Some(background) = &self.background {
            target.data_mut().copy_from_slice(background.data());
        }
        let scale = (w as f32 / DESIGN_W).min(h as f32 / DESIGN_H);
        let ox = (w as f32 - DESIGN_W * scale) / 2.0;
        let oy = (h as f32 - DESIGN_H * scale) / 2.0;
        Painter {
            pixmap: target,
            look: self,
            scale,
            ox,
            oy,
            hits: Vec::new(),
        }
    }
}

/// The art scaled to cover `w`×`h` (cut, never stretched), over `fill`.
fn cover(art: &Pixmap, w: u32, h: u32, fill: Rgb) -> Pixmap {
    let mut out = Pixmap::new(w.max(1), h.max(1)).expect("pixmap size");
    out.fill(tiny_skia::Color::from_rgba8(fill.0, fill.1, fill.2, 255));
    let scale = (w as f32 / art.width() as f32).max(h as f32 / art.height() as f32);
    let dx = (w as f32 - art.width() as f32 * scale) / 2.0;
    let dy = (h as f32 - art.height() as f32 * scale) / 2.0;
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    out.draw_pixmap(
        0,
        0,
        art.as_ref(),
        &paint,
        Transform::from_scale(scale, scale).post_translate(dx, dy),
        None,
    );
    out
}

fn paint(color: Rgb) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color.0, color.1, color.2, 255);
    p.anti_alias = true;
    p
}

/// A rectangle in device pixels, for pointer hits.
#[derive(Debug, Clone, Copy)]
pub struct Area {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Area {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// Text laid out, with its size in design units.
pub struct Laid {
    block: Block,
    pub width: f32,
    pub height: f32,
}

/// Draws in design units. Collects the areas a pointer may hit.
pub struct Painter<'a> {
    pixmap: &'a mut Pixmap,
    look: &'a mut Look,
    scale: f32,
    ox: f32,
    oy: f32,
    hits: Vec<(Area, Target)>,
}

impl Painter<'_> {
    pub fn palette(&self) -> Palette {
        self.look.brand.palette
    }

    pub fn brand(&self) -> &Brand {
        &self.look.brand
    }

    fn dx(&self, x: f32) -> f32 {
        self.ox + x * self.scale
    }

    fn dy(&self, y: f32) -> f32 {
        self.oy + y * self.scale
    }

    /// A line's thickness in device pixels: never thinner than one pixel.
    fn hairline(&self, width: f32) -> f32 {
        (width * self.scale).round().max(1.0)
    }

    /// Mark an area (design units) as what a pointer there hits.
    pub fn hit(&mut self, x: f32, y: f32, w: f32, h: f32, target: Target) {
        let area = Area {
            x0: self.dx(x),
            y0: self.dy(y),
            x1: self.dx(x + w),
            y1: self.dy(y + h),
        };
        self.hits.push((area, target));
    }

    pub fn into_hits(self) -> Vec<(Area, Target)> {
        self.hits
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        let (x0, y0) = (self.dx(x).round(), self.dy(y).round());
        let (x1, y1) = (self.dx(x + w).round(), self.dy(y + h).round());
        if let Some(rect) = Rect::from_ltrb(x0, y0, x1.max(x0 + 1.0), y1.max(y0 + 1.0)) {
            self.pixmap
                .fill_rect(rect, &paint(color), Transform::identity(), None);
        }
    }

    /// A horizontal rule of `width` design units' thickness, its top at `y`.
    pub fn rule_h(&mut self, x: f32, y: f32, w: f32, width: f32, color: Rgb) {
        let t = self.hairline(width);
        let (x0, x1, y0) = (
            self.dx(x).round(),
            self.dx(x + w).round(),
            self.dy(y).round(),
        );
        if let Some(rect) = Rect::from_ltrb(x0, y0, x1, y0 + t) {
            self.pixmap
                .fill_rect(rect, &paint(color), Transform::identity(), None);
        }
    }

    /// A vertical rule, its left at `x`.
    pub fn rule_v(&mut self, x: f32, y: f32, h: f32, width: f32, color: Rgb) {
        let t = self.hairline(width);
        let (x0, y0, y1) = (
            self.dx(x).round(),
            self.dy(y).round(),
            self.dy(y + h).round(),
        );
        if let Some(rect) = Rect::from_ltrb(x0, y0, x0 + t, y1) {
            self.pixmap
                .fill_rect(rect, &paint(color), Transform::identity(), None);
        }
    }

    /// A border inside the box, as CSS `box-sizing: border-box`.
    pub fn border(&mut self, x: f32, y: f32, w: f32, h: f32, width: f32, color: Rgb) {
        let t = self.hairline(width);
        let (x0, y0) = (self.dx(x).round(), self.dy(y).round());
        let (x1, y1) = (self.dx(x + w).round(), self.dy(y + h).round());
        let p = paint(color);
        for rect in [
            Rect::from_ltrb(x0, y0, x1, y0 + t),
            Rect::from_ltrb(x0, y1 - t, x1, y1),
            Rect::from_ltrb(x0, y0 + t, x0 + t, y1 - t),
            Rect::from_ltrb(x1 - t, y0 + t, x1, y1 - t),
        ]
        .into_iter()
        .flatten()
        {
            self.pixmap.fill_rect(rect, &p, Transform::identity(), None);
        }
    }

    /// A dashed one-unit border inside the box (CSS `1px dashed`: dashes
    /// three times the width).
    pub fn dashed_border(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        let t = self.hairline(1.0);
        let half = t / 2.0;
        let (x0, y0) = (self.dx(x).round() + half, self.dy(y).round() + half);
        let (x1, y1) = (self.dx(x + w).round() - half, self.dy(y + h).round() - half);
        let Some(rect) = Rect::from_ltrb(x0, y0, x1, y1) else {
            return;
        };
        let path = PathBuilder::from_rect(rect);
        let mut stroke = Stroke {
            width: t,
            ..Default::default()
        };
        stroke.dash = StrokeDash::new(vec![3.0 * t, 3.0 * t], 0.0);
        let mut p = paint(color);
        p.anti_alias = false;
        self.pixmap
            .stroke_path(&path, &p, &stroke, Transform::identity(), None);
    }

    /// A line icon of `size` design units with its top left at `(x, y)`.
    pub fn icon(&mut self, icon: &Icon, x: f32, y: f32, size: f32, color: Rgb) {
        let k = size * self.scale / 24.0;
        let transform = Transform::from_scale(k, k).post_translate(self.dx(x), self.dy(y));
        let stroke = Stroke {
            width: icon.stroke,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        let p = paint(color);
        for shape in icon.shapes {
            let path = match shape {
                Shape::Path(d) => svgpath::parse(d),
                Shape::Circle(cx, cy, r) => PathBuilder::from_circle(*cx, *cy, *r),
                Shape::Rect(rx, ry, rw, rh) => {
                    Rect::from_xywh(*rx, *ry, *rw, *rh).map(PathBuilder::from_rect)
                }
            };
            if let Some(path) = path {
                self.pixmap.stroke_path(&path, &p, &stroke, transform, None);
            }
        }
    }

    /// The brand's mark, `size` design units square, top left at `(x, y)`.
    pub fn mark(&mut self, x: f32, y: f32, size: f32, color: Rgb) {
        let [vx, vy, vw, vh] = self.look.brand.mark.view_box;
        let k = size * self.scale / vw.max(vh);
        let transform = Transform::from_translate(-vx, -vy)
            .post_scale(k, k)
            .post_translate(self.dx(x), self.dy(y));
        let p = paint(color);
        for (path, stroke_width, rule) in &self.look.mark {
            match stroke_width {
                None => self.pixmap.fill_path(path, &p, *rule, transform, None),
                Some(width) => {
                    let stroke = Stroke {
                        width: *width,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Default::default()
                    };
                    self.pixmap.stroke_path(path, &p, &stroke, transform, None);
                }
            }
        }
    }

    pub fn line_height(&self, style: &Style) -> f32 {
        self.look.text.line_height(style)
    }

    /// Lay text out; `max_width` in design units wraps it at words, or with
    /// `ellipsize` cuts it on one line.
    pub fn lay(
        &mut self,
        spans: &[(&str, Style)],
        max_width: Option<f32>,
        ellipsize: bool,
    ) -> Laid {
        let block = self.look.text.shape(
            spans,
            self.scale,
            max_width.map(|w| w * self.scale),
            ellipsize,
        );
        let (width, height) = (block.width / self.scale, block.height / self.scale);
        Laid {
            block,
            width,
            height,
        }
    }

    /// Draw laid text with its line box's top left at `(x, y)`.
    pub fn put(&mut self, laid: &Laid, x: f32, y: f32) {
        let (dx, dy) = (self.dx(x).round(), self.dy(y));
        self.look.text.draw(self.pixmap, &laid.block, dx, dy, None);
    }

    /// Draw laid text, keeping inside the box `(x, y, w, h)` (design units).
    pub fn put_clipped(&mut self, laid: &Laid, x: f32, y: f32, clip: (f32, f32, f32, f32)) {
        let (dx, dy) = (self.dx(x).round(), self.dy(y));
        let (cx, cy, cw, ch) = clip;
        let rect = (
            self.dx(cx).round() as i32,
            self.dy(cy).round() as i32,
            self.dx(cx + cw).round() as i32,
            self.dy(cy + ch).round() as i32,
        );
        self.look
            .text
            .draw(self.pixmap, &laid.block, dx, dy, Some(rect));
    }

    /// Lay out and draw one run of text at `(x, y)`; returns its size.
    pub fn text(&mut self, text: &str, style: Style, x: f32, y: f32) -> (f32, f32) {
        let laid = self.lay(&[(text, style)], None, false);
        self.put(&laid, x, y);
        (laid.width, laid.height)
    }
}
