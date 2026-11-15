//! The brand: what differs between LevitateOS and AcornOS on the login
//! screen. Read from a folder (default `/usr/share/reclogin`) holding
//! `brand.json`, the background image it names and, in `fonts/` beside it
//! or in `../fonts/`, the font files it names.
//!
//! `brand.json`:
//!
//! ```text
//! {
//!   "name": "LevitateOS",
//!   "palette": { "surface": "#14221A", "...": "fourteen colours, see Palette" },
//!   "mark": { "view_box": [0, 0, 24, 24],
//!             "paths": [ { "d": "M3 13…Z", "fill": true },
//!                        { "d": "M4 20…", "stroke_width": 1.9 } ] },
//!   "background": "background.png",
//!   "fonts": { "files": ["Newsreader.ttf", "…"],
//!              "serif": "Newsreader", "serif_display": "Newsreader Display",
//!              "sans": "Instrument Sans" }
//! }
//! ```
//!
//! The palette is the OS's `[look.dark]` from `distro.toml` plus three
//! colours of its own: `accent_ink` (text on the accent), `action` and
//! `on_action` (the main button).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer};

pub const DEFAULT_DIR: &str = "/usr/share/reclogin";

#[derive(Debug, Clone, Deserialize)]
pub struct Brand {
    /// The OS's name, drawn beside the mark.
    pub name: String,
    pub palette: Palette,
    pub mark: Mark,
    /// The background image's file name in the brand folder (PNG).
    pub background: String,
    pub fonts: Fonts,
    /// The folder this was read from.
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Rgb, D::Error> {
        let text = String::deserialize(deserializer)?;
        Rgb::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("{text:?} is not a colour like #1A2B3C"))
        })
    }
}

impl Rgb {
    pub fn parse(text: &str) -> Option<Rgb> {
        let hex = text.strip_prefix('#')?;
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Rgb(byte(0)?, byte(2)?, byte(4)?))
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Palette {
    pub surface: Rgb,
    pub surface_raised: Rgb,
    pub surface_sunken: Rgb,
    pub text: Rgb,
    pub text_secondary: Rgb,
    pub text_muted: Rgb,
    pub rule: Rgb,
    pub accent: Rgb,
    pub accent_ink: Rgb,
    pub action: Rgb,
    pub on_action: Rgb,
    pub bad: Rgb,
    pub warn: Rgb,
    pub ok: Rgb,
}

/// The OS's mark as SVG path data, drawn in the accent colour.
#[derive(Debug, Clone, Deserialize)]
pub struct Mark {
    /// `[min_x, min_y, width, height]` of the path coordinates.
    pub view_box: [f32; 4],
    pub paths: Vec<MarkPath>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkPath {
    /// SVG path data (`d`).
    pub d: String,
    /// Filled.
    #[serde(default)]
    pub fill: bool,
    /// Filled with the even-odd rule instead of non-zero.
    #[serde(default)]
    pub even_odd: bool,
    /// Stroked with round ends at this width (view-box units).
    #[serde(default)]
    pub stroke_width: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fonts {
    /// Font file names, looked up in `fonts/` of the brand folder and then
    /// in `../fonts/` (the development layout).
    pub files: Vec<String>,
    /// Family for the name, user names and initials.
    pub serif: String,
    /// Family for the large serif text (clock, greeting): the same face cut
    /// for large sizes. May equal `serif`.
    pub serif_display: String,
    /// Family for everything else.
    pub sans: String,
}

impl Brand {
    pub fn load(dir: &Path) -> Result<Brand> {
        let file = dir.join("brand.json");
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let mut brand: Brand =
            serde_json::from_str(&text).with_context(|| format!("reading {}", file.display()))?;
        brand.dir = dir.to_path_buf();
        if brand.mark.view_box[2] <= 0.0 || brand.mark.view_box[3] <= 0.0 {
            bail!("{}: the mark's view_box has no size", file.display());
        }
        if brand.fonts.files.is_empty() {
            bail!("{}: no font files", file.display());
        }
        Ok(brand)
    }

    pub fn background_path(&self) -> PathBuf {
        self.dir.join(&self.background)
    }

    /// Where a font file named in `fonts.files` is.
    pub fn font_path(&self, name: &str) -> Result<PathBuf> {
        let candidates = [
            self.dir.join("fonts").join(name),
            self.dir.join("..").join("fonts").join(name),
        ];
        candidates
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .with_context(|| {
                format!(
                    "font {name} is in neither {} nor {}",
                    candidates[0].display(),
                    candidates[1].display()
                )
            })
    }
}
