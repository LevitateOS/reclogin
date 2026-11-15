//! The screen: mode setting on the display reclogind handed over, two
//! kernel buffers drawn into in turn, and the pointer.
//!
//! Stage 1 drives the primary connected screen only (an internal panel if
//! there is one, else the first connected output). Other screens stay as
//! they are until the stage that adds several monitors.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use anyhow::{bail, Context, Result};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Event, Mode,
    ModeTypeFlags, PageFlipFlags,
};
use drm::Device;
use reclogin::log;
use tiny_skia::Pixmap;

const WHO: &str = "reclogin-greeter";
const CURSOR_SIZE: u32 = 64;

pub struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

struct Target {
    buffer: DumbBuffer,
    framebuffer: framebuffer::Handle,
}

pub struct Screen {
    card: Card,
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    targets: [Target; 2],
    /// The buffer on the screen.
    front: usize,
    /// A page flip is queued and not done.
    flipping: bool,
    /// The mode has been set since the last resume.
    shown: bool,
    /// The pointer as a hardware cursor, when the driver has one.
    cursor: Option<DumbBuffer>,
    cursor_tried: bool,
}

impl Screen {
    pub fn new(fd: OwnedFd) -> Result<Screen> {
        let card = Card(File::from(fd));
        let resources = card
            .resource_handles()
            .context("reading the display's resources")?;
        let connectors: Vec<connector::Info> = resources
            .connectors()
            .iter()
            .filter_map(|c| card.get_connector(*c, true).ok())
            .collect();
        let connected: Vec<&connector::Info> = connectors
            .iter()
            .filter(|c| c.state() == connector::State::Connected && !c.modes().is_empty())
            .collect();
        let internal = |c: &&&connector::Info| {
            matches!(
                c.interface(),
                connector::Interface::EmbeddedDisplayPort
                    | connector::Interface::LVDS
                    | connector::Interface::DSI
            )
        };
        let Some(info) = connected
            .iter()
            .find(internal)
            .or(connected.first())
            .copied()
        else {
            bail!("no connected screen");
        };
        let mode = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or(info.modes().first())
            .copied()
            .context("the screen offers no mode")?;

        // The pipeline already driving this connector, else one it can use.
        let current = info
            .current_encoder()
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc());
        let crtc = match current {
            Some(crtc) => crtc,
            None => info
                .encoders()
                .iter()
                .filter_map(|e| card.get_encoder(*e).ok())
                .flat_map(|e| resources.filter_crtcs(e.possible_crtcs()))
                .next()
                .context("no display pipeline for the screen")?,
        };

        let (w, h) = mode.size();
        let make = || -> Result<Target> {
            let buffer = card
                .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
                .context("making a screen buffer")?;
            let framebuffer = card
                .add_framebuffer(&buffer, 24, 32)
                .context("registering a screen buffer")?;
            Ok(Target {
                buffer,
                framebuffer,
            })
        };
        let targets = [make()?, make()?];
        log(
            WHO,
            format_args!(
                "screen {:?} at {}x{} {} Hz",
                info.interface(),
                w,
                h,
                mode.vrefresh()
            ),
        );
        Ok(Screen {
            card,
            connector: info.handle(),
            crtc,
            mode,
            targets,
            front: 0,
            flipping: false,
            shown: false,
            cursor: None,
            cursor_tried: false,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.mode.size();
        (u32::from(w), u32::from(h))
    }

    pub fn fd(&self) -> RawFd {
        self.card.0.as_raw_fd()
    }

    /// A frame can be presented now (no flip waiting).
    pub fn ready(&self) -> bool {
        !self.flipping
    }

    /// The display left: forget a flip that will never complete, and set
    /// the mode again on the next frame.
    pub fn pause(&mut self) {
        self.flipping = false;
        self.shown = false;
    }

    /// Show `frame` (the size of the screen).
    pub fn present(&mut self, frame: &Pixmap) -> Result<()> {
        let back = 1 - self.front;
        let (w, h) = self.size();
        {
            let target = &mut self.targets[back];
            let pitch = target.buffer.pitch() as usize;
            let mut map = self
                .card
                .map_dumb_buffer(&mut target.buffer)
                .context("writing the screen buffer")?;
            let out = map.as_mut();
            let src = frame.data();
            let row_bytes = w as usize * 4;
            for row in 0..h as usize {
                let line = &mut out[row * pitch..row * pitch + row_bytes];
                let from = &src[row * row_bytes..(row + 1) * row_bytes];
                for (o, i) in line
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(from.as_chunks::<4>().0)
                {
                    // Opaque RGBA to XRGB8888 (bytes B, G, R, X).
                    o[0] = i[2];
                    o[1] = i[1];
                    o[2] = i[0];
                    o[3] = 0;
                }
            }
        }
        let framebuffer = self.targets[back].framebuffer;
        if !self.shown {
            self.card
                .set_crtc(
                    self.crtc,
                    Some(framebuffer),
                    (0, 0),
                    &[self.connector],
                    Some(self.mode),
                )
                .context("setting the mode")?;
            self.shown = true;
            self.front = back;
            self.cursor_tried = false;
            return Ok(());
        }
        match self
            .card
            .page_flip(self.crtc, framebuffer, PageFlipFlags::EVENT, None)
        {
            Ok(()) => {
                self.flipping = true;
                self.front = back;
            }
            Err(e) => {
                // Some drivers cannot flip; setting the buffer works on all.
                log(
                    WHO,
                    format_args!("page flip: {e}; setting the buffer instead"),
                );
                self.card
                    .set_crtc(
                        self.crtc,
                        Some(framebuffer),
                        (0, 0),
                        &[self.connector],
                        Some(self.mode),
                    )
                    .context("showing the frame")?;
                self.front = back;
            }
        }
        Ok(())
    }

    /// Read the display's events (flip done).
    pub fn on_readable(&mut self) {
        match self.card.receive_events() {
            Ok(events) => {
                for event in events {
                    if let Event::PageFlip(_) = event {
                        self.flipping = false;
                    }
                }
            }
            Err(e) => log(WHO, format_args!("reading display events: {e}")),
        }
    }

    /// Put the pointer at `(x, y)`. Returns false when there is no hardware
    /// cursor, so the frame must draw it.
    #[allow(deprecated)] // The legacy cursor calls work on every driver that has a cursor.
    pub fn move_pointer(&mut self, x: f32, y: f32) -> bool {
        if !self.shown {
            return false;
        }
        if !self.cursor_tried {
            self.cursor_tried = true;
            self.cursor = self.make_cursor().ok();
            if let Some(buffer) = &self.cursor {
                if self
                    .card
                    .set_cursor2(self.crtc, Some(buffer), (1, 1))
                    .is_err()
                    && self.card.set_cursor(self.crtc, Some(buffer)).is_err()
                {
                    self.cursor = None;
                }
            }
        }
        if self.cursor.is_none() {
            return false;
        }
        self.card
            .move_cursor(self.crtc, (x as i32 - 1, y as i32 - 1))
            .is_ok()
    }

    fn make_cursor(&mut self) -> Result<DumbBuffer> {
        let mut buffer =
            self.card
                .create_dumb_buffer((CURSOR_SIZE, CURSOR_SIZE), DrmFourcc::Argb8888, 32)?;
        let mut image = Pixmap::new(CURSOR_SIZE, CURSOR_SIZE).context("cursor image")?;
        draw_arrow(&mut image, 1.0, 1.0);
        let pitch = buffer.pitch() as usize;
        let mut map = self.card.map_dumb_buffer(&mut buffer)?;
        let out = map.as_mut();
        for row in 0..CURSOR_SIZE as usize {
            for col in 0..CURSOR_SIZE as usize {
                let i = (row * CURSOR_SIZE as usize + col) * 4;
                let o = row * pitch + col * 4;
                let px = &image.data()[i..i + 4];
                // Premultiplied RGBA to premultiplied ARGB8888 (B, G, R, A).
                out[o] = px[2];
                out[o + 1] = px[1];
                out[o + 2] = px[0];
                out[o + 3] = px[3];
            }
        }
        drop(map);
        Ok(buffer)
    }
}

/// The pointer: a light arrow with a dark edge, its tip at `(x, y)`.
pub fn draw_arrow(pixmap: &mut Pixmap, x: f32, y: f32) {
    let mut b = tiny_skia::PathBuilder::new();
    let points = [
        (0.0, 0.0),
        (0.0, 17.0),
        (4.2, 13.2),
        (7.0, 19.5),
        (9.6, 18.4),
        (6.9, 12.3),
        (12.5, 12.3),
    ];
    b.move_to(x + points[0].0, y + points[0].1);
    for (px, py) in &points[1..] {
        b.line_to(x + px, y + py);
    }
    b.close();
    let Some(path) = b.finish() else { return };
    let mut fill = tiny_skia::Paint::default();
    fill.set_color_rgba8(242, 238, 228, 255);
    fill.anti_alias = true;
    let mut edge = fill.clone();
    edge.set_color_rgba8(20, 20, 20, 255);
    let identity = tiny_skia::Transform::identity();
    pixmap.fill_path(&path, &fill, tiny_skia::FillRule::Winding, identity, None);
    let stroke = tiny_skia::Stroke {
        width: 1.2,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(&path, &edge, &stroke, identity, None);
}
