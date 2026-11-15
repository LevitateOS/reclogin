//! The line icons of the login screens, as drawn in the design canvas
//! (24×24 view box, stroked with round ends and joins).

pub enum Shape {
    Path(&'static str),
    Circle(f32, f32, f32),
    Rect(f32, f32, f32, f32),
}

pub struct Icon {
    pub shapes: &'static [Shape],
    /// Stroke width in view-box units.
    pub stroke: f32,
}

use Shape::{Circle, Path, Rect};

pub const EYE: Icon = Icon {
    shapes: &[
        Path("M2 12s3.6-6.5 10-6.5S22 12 22 12s-3.6 6.5-10 6.5S2 12 2 12Z"),
        Circle(12.0, 12.0, 2.8),
    ],
    stroke: 1.6,
};

/// The eye with a line through it, shown while the password is visible.
pub const EYE_OFF: Icon = Icon {
    shapes: &[
        Path("M2 12s3.6-6.5 10-6.5S22 12 22 12s-3.6 6.5-10 6.5S2 12 2 12Z"),
        Circle(12.0, 12.0, 2.8),
        Path("M4 20 20 4"),
    ],
    stroke: 1.6,
};

pub const USER: Icon = Icon {
    shapes: &[
        Circle(12.0, 8.5, 3.5),
        Path("M5 20c.8-3.6 3.5-5.5 7-5.5s6.2 1.900 7 5.500"),
    ],
    stroke: 1.6,
};

pub const PLUS: Icon = Icon {
    shapes: &[Path("M12 5v14M5 12h14")],
    stroke: 1.8,
};

pub const CHEVRON_DOWN: Icon = Icon {
    shapes: &[Path("m6 9 6 6 6-6")],
    stroke: 2.0,
};

pub const KEYBOARD: Icon = Icon {
    shapes: &[
        Rect(2.5, 6.0, 19.0, 12.0),
        Path("M6 10h.01M9.5 10h.01M13 10h.01M16.5 10h.01M7.5 14h9"),
    ],
    stroke: 1.6,
};

pub const HAND: Icon = Icon {
    shapes: &[Path(
        "M9 11V5.5a1.5 1.5 0 0 1 3 0V11M12 10.5V9a1.5 1.5 0 0 1 3 0v2M15 11.5V10.5a1.5 1.5 0 0 1 3 0V15a6 6 0 0 1-6 6h-1a5 5 0 0 1-4.2-2.3L4 14.5a1.5 1.5 0 0 1 2.5-1.7L9 15",
    )],
    stroke: 1.6,
};

pub const MOON: Icon = Icon {
    shapes: &[Path(
        "M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5Z",
    )],
    stroke: 1.6,
};

pub const RESTART: Icon = Icon {
    shapes: &[Path("M20 12a8 8 0 1 1-2.6-5.9"), Path("M20 4v5h-5")],
    stroke: 1.6,
};

pub const POWER: Icon = Icon {
    shapes: &[Path("M12 3v9"), Path("M6.3 6.8a8 8 0 1 0 11.4 0")],
    stroke: 1.6,
};

pub const ALERT: Icon = Icon {
    shapes: &[Circle(12.0, 12.0, 9.0), Path("M12 7.5v5.5M12 16.5h.01")],
    stroke: 1.8,
};

pub const CAPS: Icon = Icon {
    shapes: &[Path("M12 4 4.5 12H9v4h6v-4h4.5L12 4Z"), Path("M9 19.5h6")],
    stroke: 1.8,
};

/// The tick before a step reclogind confirms ("Current password accepted.").
pub const CHECK: Icon = Icon {
    shapes: &[Path("m5 12.5 4.5 4.5L19 7.5")],
    stroke: 2.0,
};

pub const CHEVRON_LEFT: Icon = Icon {
    shapes: &[Path("m15 6-6 6 6 6")],
    stroke: 2.0,
};

/// The plus of the chosen "Other user…" row (a heavier stroke).
pub const PLUS_BOLD: Icon = Icon {
    shapes: &[Path("M12 5v14M5 12h14")],
    stroke: 2.0,
};

/// The guest's mark on the chosen guest row (a heavier stroke).
pub const USER_BOLD: Icon = Icon {
    shapes: &[
        Circle(12.0, 8.5, 3.5),
        Path("M5 20c.8-3.6 3.5-5.5 7-5.5s6.2 1.900 7 5.500"),
    ],
    stroke: 1.8,
};

/// The session menu's chevron while the menu is open.
pub const CHEVRON_UP: Icon = Icon {
    shapes: &[Path("m6 15 6-6 6 6")],
    stroke: 2.0,
};
