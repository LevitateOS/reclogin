//! The screens: mode setting on the display reclogind handed over, two
//! kernel buffers per screen drawn into in turn, and the pointer.
//!
//! Every connected screen gets a mode and buffers. One of them is the
//! primary: the first built-in panel (eDP, LVDS, DSI) if there is one, else
//! the first connected screen in the kernel's order. It shows the login and
//! gets the pointer. The others show a picture drawn once ([`Screen::show_other`]).
//!
//! The kernel reports a plugged or removed screen by a uevent on a netlink
//! socket, which the greeter cannot open (its lock-down refuses every new
//! socket), and not on the display handle (that carries only page-flip
//! events). So [`Screen::rescan`] asks the kernel for the connectors again,
//! without probing the hardware (`force` false: the kernel's own hotplug
//! handling keeps that state), every few seconds.
//!
//! Power: the connector's `DPMS` property (the legacy property every driver
//! keeps, atomic ones too) is set to Off and back to On. A connector without
//! it has its pipeline switched off instead and the mode is set again on
//! waking.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use anyhow::{bail, Context, Result};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, property, Device as ControlDevice, Event,
    Mode, ModeTypeFlags, PageFlipFlags,
};
use drm::Device;
use reclogin::log;
use tiny_skia::Pixmap;

const WHO: &str = "reclogin-greeter";
const CURSOR_SIZE: u32 = 64;
/// `DRM_MODE_DPMS_ON` and `DRM_MODE_DPMS_OFF`.
const DPMS_ON: u64 = 0;
const DPMS_OFF: u64 = 3;

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

/// What a secondary screen shows: the brand's name and the size it was
/// drawn at.
pub type Drawn = (String, u32, u32);

struct Output {
    connector: connector::Handle,
    interface: connector::Interface,
    /// Position among the kernel's connectors (the order the primary is
    /// chosen in).
    order: usize,
    crtc: crtc::Handle,
    mode: Mode,
    targets: [Target; 2],
    /// The buffer on the screen.
    front: usize,
    /// A page flip is queued and not done.
    flipping: bool,
    /// The mode has been set since the last pause or power-off.
    shown: bool,
    /// The pointer as a hardware cursor, when the driver has one.
    cursor: Option<DumbBuffer>,
    cursor_tried: bool,
    /// The connector's `DPMS` property.
    dpms: Option<property::Handle>,
    /// What the buffer on the screen holds while this is not the primary.
    drawn: Option<Drawn>,
}

impl Output {
    fn internal(&self) -> bool {
        is_internal(self.interface)
    }

    fn size(&self) -> (u32, u32) {
        let (w, h) = self.mode.size();
        (u32::from(w), u32::from(h))
    }
}

fn is_internal(interface: connector::Interface) -> bool {
    matches!(
        interface,
        connector::Interface::EmbeddedDisplayPort
            | connector::Interface::LVDS
            | connector::Interface::DSI
    )
}

/// What a [`Screen::rescan`] changed.
#[derive(Default, Clone, Copy)]
pub struct Changes {
    /// Another screen is the primary now (or the first one appeared): the
    /// frame has another size and the pointer another box.
    pub primary: bool,
    /// A secondary screen came or went (or the primary changed).
    pub others: bool,
}

pub struct Screen {
    card: Card,
    outputs: Vec<Output>,
    primary: usize,
    /// The screens are switched off (idle).
    asleep: bool,
}

impl Screen {
    pub fn new(fd: OwnedFd) -> Result<Screen> {
        let mut screen = Screen {
            card: Card(File::from(fd)),
            outputs: Vec::new(),
            primary: 0,
            asleep: false,
        };
        screen.rescan(true)?;
        if screen.outputs.is_empty() {
            bail!("no connected screen");
        }
        Ok(screen)
    }

    /// Look at the connectors again: bring up screens that appeared, let go
    /// of those that went, and choose the primary again. `force` probes the
    /// hardware (at start); later scans only read what the kernel knows.
    /// When no screen is connected any more the last ones are kept (there
    /// is nothing to move the login to).
    pub fn rescan(&mut self, force: bool) -> Result<Changes> {
        let resources = self
            .card
            .resource_handles()
            .context("reading the display's resources")?;
        let mut connected = Vec::new();
        for (order, handle) in resources.connectors().iter().enumerate() {
            let Ok(info) = self.card.get_connector(*handle, force) else {
                continue;
            };
            if info.state() == connector::State::Connected && !info.modes().is_empty() {
                connected.push((order, info));
            }
        }
        if connected.is_empty() {
            return Ok(Changes::default());
        }
        let before: Vec<connector::Handle> = self.outputs.iter().map(|o| o.connector).collect();
        let old_primary = self.outputs.get(self.primary).map(|o| o.connector);

        // Let go of what is gone.
        let mut kept = Vec::new();
        for output in std::mem::take(&mut self.outputs) {
            if connected
                .iter()
                .any(|(_, i)| i.handle() == output.connector)
            {
                kept.push(output);
            } else {
                log(WHO, format_args!("screen {:?} removed", output.interface));
                self.release(output);
            }
        }
        self.outputs = kept;

        // Bring up what is new.
        for (order, info) in &connected {
            if self.outputs.iter().any(|o| o.connector == info.handle()) {
                continue;
            }
            match self.bring_up(&resources, info, *order) {
                Ok(output) => self.outputs.push(output),
                Err(e) => log(
                    WHO,
                    format_args!("screen {:?} not used: {e:#}", info.interface()),
                ),
            }
        }
        if self.outputs.is_empty() {
            bail!("no connected screen could be used");
        }
        self.outputs.sort_by_key(|o| o.order);

        // The primary: the first built-in panel, else the first in order.
        let primary = self.outputs.iter().position(Output::internal).unwrap_or(0);
        let new_primary = self.outputs[primary].connector;
        let primary_changed = old_primary != Some(new_primary);
        if primary_changed {
            // The old primary (if it is still here) no longer shows the
            // pointer; the new one starts clean.
            if let Some(old) = old_primary {
                let card = &self.card;
                if let Some(o) = self.outputs.iter_mut().find(|o| o.connector == old) {
                    hide_cursor(card, o);
                    o.drawn = None;
                    o.shown = false;
                }
            }
            let o = &mut self.outputs[primary];
            o.cursor_tried = false;
            o.flipping = false;
            log(
                WHO,
                format_args!(
                    "the login is on screen {:?} ({}x{})",
                    o.interface,
                    o.size().0,
                    o.size().1
                ),
            );
        }
        self.primary = primary;
        let after: Vec<connector::Handle> = self.outputs.iter().map(|o| o.connector).collect();
        let mut sorted_before = before;
        let mut sorted_after = after;
        sorted_before.sort_by_key(|h| u32::from(*h));
        sorted_after.sort_by_key(|h| u32::from(*h));
        Ok(Changes {
            primary: primary_changed,
            others: primary_changed || sorted_before != sorted_after,
        })
    }

    /// Give back a screen's buffers and pipeline.
    fn release(&mut self, mut output: Output) {
        // The kernel keeps the pipeline bound to an unplugged connector
        // until it is told otherwise.
        let _ = self.card.set_crtc(output.crtc, None, (0, 0), &[], None);
        hide_cursor(&self.card, &mut output);
        if let Some(cursor) = output.cursor.take() {
            let _ = self.card.destroy_dumb_buffer(cursor);
        }
        for target in output.targets {
            let _ = self.card.destroy_framebuffer(target.framebuffer);
            let _ = self.card.destroy_dumb_buffer(target.buffer);
        }
    }

    /// A mode, a pipeline of its own and two buffers for a connector.
    fn bring_up(
        &self,
        resources: &drm::control::ResourceHandles,
        info: &connector::Info,
        order: usize,
    ) -> Result<Output> {
        let card = &self.card;
        let mode = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or(info.modes().first())
            .copied()
            .context("the screen offers no mode")?;
        let used: Vec<crtc::Handle> = self.outputs.iter().map(|o| o.crtc).collect();
        // The pipeline already driving this connector, else one it can use;
        // never one another screen has.
        let current = info
            .current_encoder()
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc())
            .filter(|c| !used.contains(c));
        let crtc = match current {
            Some(crtc) => crtc,
            None => info
                .encoders()
                .iter()
                .filter_map(|e| card.get_encoder(*e).ok())
                .flat_map(|e| resources.filter_crtcs(e.possible_crtcs()))
                .find(|c| !used.contains(c))
                .context("no free display pipeline for the screen")?,
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
        let dpms = dpms_property(card, info.handle());
        log(
            WHO,
            format_args!(
                "screen {:?} at {}x{} {} Hz{}",
                info.interface(),
                w,
                h,
                mode.vrefresh(),
                if dpms.is_some() {
                    ""
                } else {
                    " (no DPMS property)"
                }
            ),
        );
        Ok(Output {
            connector: info.handle(),
            interface: info.interface(),
            order,
            crtc,
            mode,
            targets,
            front: 0,
            flipping: false,
            shown: false,
            cursor: None,
            cursor_tried: false,
            dpms,
            drawn: None,
        })
    }

    /// The primary screen's size: the frame the login is drawn into.
    pub fn size(&self) -> (u32, u32) {
        self.outputs[self.primary].size()
    }

    pub fn fd(&self) -> RawFd {
        self.card.0.as_raw_fd()
    }

    /// A frame can be presented now (no flip waiting).
    pub fn ready(&self) -> bool {
        !self.outputs[self.primary].flipping
    }

    /// The display left: forget flips that will never complete, and set the
    /// modes again on the next frames.
    pub fn pause(&mut self) {
        for o in &mut self.outputs {
            o.flipping = false;
            o.shown = false;
        }
    }

    /// Switch every screen off or on (idle). A screen without the `DPMS`
    /// property has its pipeline disabled and its mode set again by the next
    /// frame.
    pub fn set_power(&mut self, on: bool) {
        self.asleep = !on;
        let card = &self.card;
        for o in &mut self.outputs {
            o.flipping = false;
            match o.dpms {
                Some(prop) => {
                    let value = if on { DPMS_ON } else { DPMS_OFF };
                    if let Err(e) = card.set_property(o.connector, prop, value) {
                        log(WHO, format_args!("DPMS of {:?}: {e}", o.interface));
                    }
                }
                None if !on => {
                    let _ = card.set_crtc(o.crtc, None, (0, 0), &[], None);
                    o.shown = false;
                }
                None => {}
            }
        }
    }

    /// Show `frame` (the size of the primary screen).
    pub fn present(&mut self, frame: &Pixmap) -> Result<()> {
        let card = &self.card;
        let o = &mut self.outputs[self.primary];
        let back = 1 - o.front;
        let size = o.size();
        write_frame(card, &mut o.targets[back], size, frame)?;
        let framebuffer = o.targets[back].framebuffer;
        if !o.shown {
            card.set_crtc(
                o.crtc,
                Some(framebuffer),
                (0, 0),
                &[o.connector],
                Some(o.mode),
            )
            .context("setting the mode")?;
            powered_on(card, o);
            o.shown = true;
            o.front = back;
            o.cursor_tried = false;
            return Ok(());
        }
        match card.page_flip(o.crtc, framebuffer, PageFlipFlags::EVENT, None) {
            Ok(()) => {
                o.flipping = true;
                o.front = back;
            }
            Err(e) => {
                // Some drivers cannot flip; setting the buffer works on all.
                log(
                    WHO,
                    format_args!("page flip: {e}; setting the buffer instead"),
                );
                card.set_crtc(
                    o.crtc,
                    Some(framebuffer),
                    (0, 0),
                    &[o.connector],
                    Some(o.mode),
                )
                .context("showing the frame")?;
                o.front = back;
            }
        }
        Ok(())
    }

    /// The secondary screens, by index, with their sizes.
    pub fn secondaries(&self) -> Vec<(usize, (u32, u32))> {
        self.outputs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.primary)
            .map(|(i, o)| (i, o.size()))
            .collect()
    }

    /// Whether secondary `index` shows something else than `drawn`.
    pub fn needs_picture(&self, index: usize, drawn: &Drawn) -> bool {
        self.outputs[index].drawn.as_ref() != Some(drawn)
    }

    /// Put `picture` (the size of the screen) on secondary `index`.
    pub fn show_other(&mut self, index: usize, drawn: Drawn, picture: &Pixmap) {
        let card = &self.card;
        let o = &mut self.outputs[index];
        o.drawn = Some(drawn);
        let front = o.front;
        let size = o.size();
        if let Err(e) = write_frame(card, &mut o.targets[front], size, picture) {
            log(
                WHO,
                format_args!("drawing on screen {:?}: {e:#}", o.interface),
            );
            return;
        }
        o.shown = false;
    }

    /// Set the mode again on every secondary screen that shows a picture
    /// but lost its mode (after a pause or a power-off). Not while asleep.
    pub fn reshow_others(&mut self) {
        if self.asleep {
            return;
        }
        let card = &self.card;
        for (i, o) in self.outputs.iter_mut().enumerate() {
            if i == self.primary || o.shown || o.drawn.is_none() {
                continue;
            }
            let framebuffer = o.targets[o.front].framebuffer;
            if let Err(e) = card.set_crtc(
                o.crtc,
                Some(framebuffer),
                (0, 0),
                &[o.connector],
                Some(o.mode),
            ) {
                log(
                    WHO,
                    format_args!("setting the mode of {:?}: {e}", o.interface),
                );
            } else {
                powered_on(card, o);
            }
            // Not tried again until the next pause or power-off.
            o.shown = true;
        }
    }

    /// Read the display's events (flip done).
    pub fn on_readable(&mut self) {
        match self.card.receive_events() {
            Ok(events) => {
                for event in events {
                    if let Event::PageFlip(done) = event {
                        for o in self.outputs.iter_mut().filter(|o| o.crtc == done.crtc) {
                            o.flipping = false;
                        }
                    }
                }
            }
            Err(e) => log(WHO, format_args!("reading display events: {e}")),
        }
    }

    /// Put the pointer at `(x, y)` on the primary screen. Returns false when
    /// there is no hardware cursor, so the frame must draw it.
    #[allow(deprecated)] // The legacy cursor calls work on every driver that has a cursor.
    pub fn move_pointer(&mut self, x: f32, y: f32) -> bool {
        let o = &mut self.outputs[self.primary];
        if !o.shown {
            return false;
        }
        if !o.cursor_tried {
            o.cursor_tried = true;
            let buffer = match o.cursor.take() {
                Some(buffer) => Some(buffer),
                None => make_cursor(&self.card).ok(),
            };
            o.cursor = buffer;
            if let Some(buffer) = &o.cursor {
                if self.card.set_cursor2(o.crtc, Some(buffer), (1, 1)).is_err()
                    && self.card.set_cursor(o.crtc, Some(buffer)).is_err()
                {
                    o.cursor = None;
                }
            }
        }
        if o.cursor.is_none() {
            return false;
        }
        self.card
            .move_cursor(o.crtc, (x as i32 - 1, y as i32 - 1))
            .is_ok()
    }

    /// Take the hardware cursor off the primary screen (a touch is in use);
    /// the next [`Screen::move_pointer`] puts it back.
    pub fn hide_pointer(&mut self) {
        let card = &self.card;
        hide_cursor(card, &mut self.outputs[self.primary]);
    }
}

/// Switch a connector's power on after its mode was set (legacy drivers do
/// not do it by themselves after an Off).
fn powered_on(card: &Card, o: &Output) {
    if let Some(prop) = o.dpms {
        let _ = card.set_property(o.connector, prop, DPMS_ON);
    }
}

#[allow(deprecated)]
fn hide_cursor(card: &Card, o: &mut Output) {
    if o.cursor_tried && o.cursor.is_some() {
        let _ = card.set_cursor(o.crtc, None::<&DumbBuffer>);
    }
    // The buffer is kept; the next move puts it back.
    o.cursor_tried = false;
}

/// The connector's `DPMS` property, if it has one.
fn dpms_property(card: &Card, connector: connector::Handle) -> Option<property::Handle> {
    let set = card.get_properties(connector).ok()?;
    let (handles, _) = set.as_props_and_values();
    handles.iter().copied().find(|h| {
        card.get_property(*h)
            .is_ok_and(|info| info.name().to_bytes() == b"DPMS")
    })
}

/// Copy `frame` (opaque RGBA, `size`) into a screen buffer as XRGB8888.
fn write_frame(card: &Card, target: &mut Target, size: (u32, u32), frame: &Pixmap) -> Result<()> {
    let (w, h) = size;
    if frame.width() != w || frame.height() != h {
        bail!(
            "the frame is {}x{}, the screen {w}x{h}",
            frame.width(),
            frame.height()
        );
    }
    let pitch = target.buffer.pitch() as usize;
    let mut map = card
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
    Ok(())
}

fn make_cursor(card: &Card) -> Result<DumbBuffer> {
    let mut buffer =
        card.create_dumb_buffer((CURSOR_SIZE, CURSOR_SIZE), DrmFourcc::Argb8888, 32)?;
    let mut image = Pixmap::new(CURSOR_SIZE, CURSOR_SIZE).context("cursor image")?;
    draw_arrow(&mut image, 1.0, 1.0);
    let pitch = buffer.pitch() as usize;
    let mut map = card.map_dumb_buffer(&mut buffer)?;
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
