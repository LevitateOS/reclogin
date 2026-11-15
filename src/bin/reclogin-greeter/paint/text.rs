//! Text: shaping with cosmic-text, using only the brand's fonts, and
//! drawing the glyphs into a pixmap.

use anyhow::{bail, Context, Result};
use cosmic_text::{
    Attrs, Buffer, Color, Ellipsize, EllipsizeHeightLimit, Family, FeatureTag, FontFeatures,
    FontSystem, Metrics, Shaping, Style as FontStyle, SwashCache, SwashContent, Weight, Wrap,
};
use reclogin::brand::{Brand, Rgb};
use tiny_skia::Pixmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    /// The brand's serif (name, user names, initials).
    Serif,
    /// The serif cut for large sizes (clock, greeting).
    SerifDisplay,
    Sans,
}

/// How a run of text looks. Sizes are in design units.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub face: Face,
    pub size: f32,
    pub weight: u16,
    pub italic: bool,
    /// Letter spacing in em, as CSS `letter-spacing`.
    pub tracking: f32,
    pub color: Rgb,
    /// Line height as a multiple of the size; `None` is the font's own.
    pub line_height: Option<f32>,
    /// Figures of equal width.
    pub tabular: bool,
}

impl Style {
    pub fn new(face: Face, size: f32, color: Rgb) -> Style {
        Style {
            face,
            size,
            weight: 400,
            italic: false,
            tracking: 0.0,
            color,
            line_height: None,
            tabular: false,
        }
    }

    pub fn weight(mut self, weight: u16) -> Style {
        self.weight = weight;
        self
    }

    pub fn italic(mut self) -> Style {
        self.italic = true;
        self
    }

    pub fn tracking(mut self, em: f32) -> Style {
        self.tracking = em;
        self
    }

    pub fn line_height(mut self, multiple: f32) -> Style {
        self.line_height = Some(multiple);
        self
    }

    pub fn with_color(mut self, color: Rgb) -> Style {
        self.color = color;
        self
    }

    pub fn tabular(mut self) -> Style {
        self.tabular = true;
        self
    }
}

/// Shaped text, ready to draw. Sizes in device pixels.
pub struct Block {
    buffer: Buffer,
    pub width: f32,
    pub height: f32,
}

pub struct Text {
    system: FontSystem,
    cache: SwashCache,
    families: [String; 3],
    /// The font's own line height, as a multiple of the size.
    normal: [f32; 3],
}

fn index(face: Face) -> usize {
    match face {
        Face::Serif => 0,
        Face::SerifDisplay => 1,
        Face::Sans => 2,
    }
}

impl Text {
    /// Load the brand's fonts, and only those.
    pub fn load(brand: &Brand) -> Result<Text> {
        let mut db = fontdb::Database::new();
        for name in &brand.fonts.files {
            let path = brand.font_path(name)?;
            let data =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            db.load_font_data(data);
        }
        let families = [
            brand.fonts.serif.clone(),
            brand.fonts.serif_display.clone(),
            brand.fonts.sans.clone(),
        ];
        let mut normal = [1.2f32; 3];
        for (i, family) in families.iter().enumerate() {
            let face = db
                .faces()
                .find(|f| {
                    f.families.iter().any(|(name, _)| name == family)
                        && f.style == fontdb::Style::Normal
                })
                .or_else(|| {
                    db.faces()
                        .find(|f| f.families.iter().any(|(name, _)| name == family))
                });
            let Some(face) = face else {
                bail!("no font file of the brand has the family {family:?}");
            };
            let ratio = db.with_face_data(face.id, |data, index| {
                let face = ttf_parser::Face::parse(data, index).ok()?;
                let units = f32::from(face.units_per_em());
                let height = f32::from(face.ascender()) - f32::from(face.descender())
                    + f32::from(face.line_gap());
                Some(height / units)
            });
            if let Some(Some(ratio)) = ratio {
                normal[i] = ratio;
            }
        }
        let system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        Ok(Text {
            system,
            cache: SwashCache::new(),
            families,
            normal,
        })
    }

    /// The line height of `style`, in design units.
    pub fn line_height(&self, style: &Style) -> f32 {
        style.line_height.unwrap_or(self.normal[index(style.face)]) * style.size
    }

    /// Shape `spans` at `scale` device pixels per design unit. With a
    /// `max_width` (device pixels) the text wraps at words, or, with
    /// `ellipsize`, is cut with an ellipsis on one line.
    pub fn shape(
        &mut self,
        spans: &[(&str, Style)],
        scale: f32,
        max_width: Option<f32>,
        ellipsize: bool,
    ) -> Block {
        let first =
            spans
                .first()
                .map(|(_, s)| *s)
                .unwrap_or(Style::new(Face::Sans, 16.0, Rgb(0, 0, 0)));
        let metrics = Metrics::new(first.size * scale, self.line_height(&first) * scale);
        let mut buffer = Buffer::new(&mut self.system, metrics);
        let families = self.families.clone();
        let normal = self.normal;
        let line_height = move |style: &Style| {
            style.line_height.unwrap_or(normal[index(style.face)]) * style.size
        };
        let attrs = |style: &Style| {
            let mut features = FontFeatures::new();
            if style.tabular {
                features.enable(FeatureTag::new(b"tnum"));
            }
            let Rgb(r, g, b) = style.color;
            Attrs::new()
                .family(Family::Name(&families[index(style.face)]))
                .weight(Weight(style.weight))
                .style(if style.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                })
                .letter_spacing(style.tracking)
                .color(Color::rgb(r, g, b))
                .font_features(features)
                .metrics(Metrics::new(style.size * scale, line_height(style) * scale))
        };
        {
            let mut b = buffer.borrow_with(&mut self.system);
            if ellipsize {
                b.set_wrap(Wrap::None);
                b.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
            } else {
                b.set_wrap(if max_width.is_some() {
                    Wrap::Word
                } else {
                    Wrap::None
                });
            }
            b.set_size(max_width, None);
            let default = attrs(&first);
            b.set_rich_text(
                spans.iter().map(|(text, style)| (*text, attrs(style))),
                &default,
                Shaping::Advanced,
                None,
            );
            b.shape_until_scroll(false);
        }
        let mut width: f32 = 0.0;
        let mut height: f32 = 0.0;
        for run in buffer.layout_runs() {
            width = width.max(run.line_w);
            height = height.max(run.line_top + run.line_height);
        }
        Block {
            buffer,
            width,
            height,
        }
    }

    /// Draw `block` with its top left corner at `(x, y)` (device pixels),
    /// keeping inside `clip` (x0, y0, x1, y1) when given.
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        block: &Block,
        x: f32,
        y: f32,
        clip: Option<(i32, i32, i32, i32)>,
    ) {
        let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
        let (cx0, cy0, cx1, cy1) = match clip {
            Some((x0, y0, x1, y1)) => (x0.max(0), y0.max(0), x1.min(pw), y1.min(ph)),
            None => (0, 0, pw, ph),
        };
        let data = pixmap.data_mut();
        for run in block.buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((x, y + run.line_y), 1.0);
                let color = glyph.color_opt.unwrap_or(Color::rgb(255, 255, 255));
                let Some(image) = self.cache.get_image(&mut self.system, physical.cache_key) else {
                    continue;
                };
                let left = physical.x + image.placement.left;
                let top = physical.y - image.placement.top;
                let (w, h) = (image.placement.width as i32, image.placement.height as i32);
                for row in 0..h {
                    let py = top + row;
                    if py < cy0 || py >= cy1 {
                        continue;
                    }
                    for col in 0..w {
                        let px = left + col;
                        if px < cx0 || px >= cx1 {
                            continue;
                        }
                        let i = (row * w + col) as usize;
                        let (src, cover) = match image.content {
                            SwashContent::Mask => {
                                ([color.r(), color.g(), color.b()], image.data[i])
                            }
                            SwashContent::Color => {
                                let j = i * 4;
                                (
                                    [image.data[j], image.data[j + 1], image.data[j + 2]],
                                    image.data[j + 3],
                                )
                            }
                            SwashContent::SubpixelMask => continue,
                        };
                        if cover == 0 {
                            continue;
                        }
                        let a = u32::from(cover);
                        let o = ((py * pw + px) * 4) as usize;
                        for c in 0..3 {
                            data[o + c] = ((u32::from(src[c]) * a
                                + u32::from(data[o + c]) * (255 - a))
                                / 255) as u8;
                        }
                        data[o + 3] = ((255 * a + u32::from(data[o + 3]) * (255 - a)) / 255) as u8;
                    }
                }
            }
        }
    }
}
