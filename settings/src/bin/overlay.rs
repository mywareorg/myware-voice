//! The listening popup: a layer-shell surface that shows what the microphone is
//! hearing while a capture is running.
//!
//! It is drawn by us rather than by a toolkit because the surface must be a
//! `zwlr_layer_surface_v1` with `keyboard_interactivity = none`: any ordinary
//! window would take focus, and a focused overlay is exactly the failure the whole
//! application is built to avoid — the paste would land in the overlay.
//!
//! The daemon owns this process. It starts it when a capture starts, writes one
//! JSON line per tick on stdin, and closes the pipe on stop, which is this
//! program's only shutdown signal. It draws nothing without a compositor that
//! speaks `wlr-layer-shell`, and a failure there costs the popup and nothing else.

use std::io::{BufRead, BufReader};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::{slot::SlotPool, Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
};

/// The panel, in logical pixels: one microphone glyph and nothing else.
const WIDTH: u32 = 104;
const HEIGHT: u32 = 86;
/// Distance from the top and right edges of the output. The top one clears a
/// status bar: anchors are measured from the output's edge, not from the usable
/// area, so 18 would sit on top of waybar's widgets.
const MARGIN_TOP: i32 = 46;
const MARGIN_RIGHT: i32 = 18;

/// One tick of the daemon's status, as it arrives on stdin.
#[derive(Debug, Default, serde::Deserialize)]
struct Tick {
    #[serde(default)]
    level: f32,
    #[serde(default)]
    streaming: bool,
}

fn main() -> anyhow::Result<()> {
    let connection = wayland_client::Connection::connect_to_env()
        .map_err(|error| anyhow::anyhow!("no Wayland session: {error}"))?;
    let (globals, mut queue) = wayland_client::globals::registry_queue_init::<App>(&connection)?;
    let handle = queue.handle();

    let compositor = CompositorState::bind(&globals, &handle)?;
    let layer_shell = LayerShell::bind(&globals, &handle)?;
    let shm = Shm::bind(&globals, &handle)?;

    let surface = compositor.create_surface(&handle);
    let layer = layer_shell.create_layer_surface(
        &handle,
        surface,
        Layer::Overlay,
        Some("hippo-dictate-overlay"),
        None,
    );
    // Top right of the focused output, which is where the eye already is and
    // where nothing the compositor draws (bars, notifications) usually sits. No
    // output is named, so the compositor puts it on the one with focus.
    layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
    layer.set_margin(MARGIN_TOP, MARGIN_RIGHT, 0, 0);
    layer.set_size(WIDTH, HEIGHT);
    // The whole point: an overlay that cannot take the keyboard.
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Do not reserve space, and do not move the windows around it.
    layer.set_exclusive_zone(-1);
    layer.commit();

    let pool = SlotPool::new((WIDTH * HEIGHT * 4) as usize, &shm)?;
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &handle),
        shm,
        pool,
        layer,
        tick: Tick::default(),
        fill: 0.0,
        configured: false,
        exit: false,
    };

    // The daemon writes one line per tick and closes the pipe to say "done". A
    // reader thread turns that into a channel so the Wayland queue keeps being
    // pumped while we wait for it.
    let (sender, receiver) = mpsc::channel::<Tick>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin.lock()).lines() {
            let Ok(line) = line else { break };
            if let Ok(tick) = serde_json::from_str::<Tick>(&line) {
                if sender.send(tick).is_err() {
                    return;
                }
            }
        }
    });

    // Nothing is painted before the first configure; there is no buffer to paint.
    while !app.configured {
        queue.blocking_dispatch(&mut app)?;
    }

    loop {
        match receiver.recv_timeout(Duration::from_millis(80)) {
            Ok(tick) => {
                app.tick = tick;
                app.draw();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Wayland events still matter (a closed surface, a scale change), so drain
        // them without blocking.
        connection.flush()?;
        if let Some(read) = queue.prepare_read() {
            let _ = read.read();
        }
        queue.dispatch_pending(&mut app)?;
        if app.exit {
            break;
        }
    }
    Ok(())
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    tick: Tick,
    /// Smoothed fill, so the glyph rises at once and falls gently.
    fill: f32,
    configured: bool,
    exit: bool,
}

impl App {
    /// Paint the panel and commit. Called once per tick, which is ten times a
    /// second: small enough that redrawing from scratch is cheaper than tracking
    /// damage.
    fn draw(&mut self) {
        let width = WIDTH as i32;
        let height = HEIGHT as i32;
        let stride = width * 4;
        let (buffer, canvas) = match self.pool.create_buffer(
            width,
            height,
            stride,
            wayland_client::protocol::wl_shm::Format::Argb8888,
        ) {
            Ok(pair) => pair,
            Err(_) => return,
        };

        // A meter that follows the sample exactly flickers; a slow release keeps
        // the glyph readable while still dropping when the room goes quiet.
        let target = fill_fraction(self.tick.level);
        self.fill = target.max(self.fill * 0.86);
        paint(canvas, WIDTH, HEIGHT, &self.tick, self.fill);

        buffer.attach_to(self.layer.wl_surface()).ok();
        self.layer.commit();
    }
}

/// Draw the panel: a rounded dark background and a microphone that fills with
/// the input level from the bottom up.
///
/// A row of bars was too small a delta to read at a glance — a level meter's job
/// is to answer "is it hearing me" in peripheral vision, and 36 columns moving by
/// two pixels does not. A shape that fills does: the eye reads how much of the
/// glyph is lit, not the position of a bar.
fn paint(canvas: &mut [u8], width: u32, height: u32, tick: &Tick, fill: f32) {
    let mut canvas = Canvas {
        pixels: canvas,
        width,
        height,
    };
    canvas.rounded_panel(12.0);
    canvas.microphone(tick.streaming, fill.clamp(0.0, 1.0));
}

/// The pixel buffer, with the two shapes this panel needs.
struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
}

/// The glyph, in pixels from the panel's top left. A capsule, the cradle it sits
/// in, and a stem on a base: the shape everyone recognises as a microphone.
const CAPSULE: (f32, f32, f32, f32) = (52.0, 8.0, 10.0, 34.0); // x, y, half-width, height
const CRADLE: (f32, f32, f32, f32) = (52.0, 39.0, 15.0, 2.6); // cx, cy, radius, thickness
const STEM: (f32, f32, f32, f32) = (50.5, 54.0, 3.0, 9.0); // x, y, width, height
const BASE: (f32, f32, f32, f32) = (42.0, 63.0, 20.0, 3.0); // x, y, width, height
/// The glyph's vertical extent, which is what the fill traverses.
const GLYPH_TOP: f32 = 8.0;
const GLYPH_BOTTOM: f32 = 66.0;

impl Canvas<'_> {
    fn rounded_panel(&mut self, radius: f32) {
        let (panel_r, panel_g, panel_b) = (28u8, 28u8, 32u8);
        let (width, height) = (self.width, self.height);
        for y in 0..height {
            for x in 0..width {
                let alpha = corner_alpha(x as f32, y as f32, width, height, radius);
                let offset = ((y * width + x) * 4) as usize;
                self.pixels[offset] = ((panel_b as u16 * alpha as u16) / 255) as u8;
                self.pixels[offset + 1] = ((panel_g as u16 * alpha as u16) / 255) as u8;
                self.pixels[offset + 2] = ((panel_r as u16 * alpha as u16) / 255) as u8;
                self.pixels[offset + 3] = alpha;
            }
        }
    }

    /// The fill. The whole glyph — capsule, stem and base — fills from its bottom
    /// up, so the base lights first, then the stem, then the capsule. A fill that
    /// started at the capsule wasted most of its travel on normal speech levels.
    fn microphone(&mut self, streaming: bool, fill: f32) {
        // Mode, as before: green for ordinary dictation, blue for streaming.
        let lit = if streaming {
            (91, 155, 213)
        } else {
            (76, 175, 122)
        };
        let track = (58, 60, 68);
        let line = GLYPH_BOTTOM - fill * (GLYPH_BOTTOM - GLYPH_TOP);

        for y in 0..self.height {
            for x in 0..self.width {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                if !in_capsule(px, py) && !in_stand(px, py) {
                    continue;
                }
                let colour = if py >= line { lit } else { track };
                let offset = ((y * self.width + x) * 4) as usize;
                self.pixels[offset] = colour.2;
                self.pixels[offset + 1] = colour.1;
                self.pixels[offset + 2] = colour.0;
                self.pixels[offset + 3] = 255;
            }
        }
    }
}

/// How full the glyph should be for an RMS level.
///
/// In dB, because loudness is: -40 dB (a quiet room, RMS 0.01) is empty and -12 dB
/// (RMS 0.25) is full. A linear mapping spent most of its travel on levels speech
/// never reaches, which is why the glyph looked frozen at a fifth of its height.
fn fill_fraction(level: f32) -> f32 {
    let db = 20.0 * level.max(1e-6).log10();
    ((db + 40.0) / 28.0).clamp(0.0, 1.0)
}

/// Is this point inside the capsule?
fn in_capsule(x: f32, y: f32) -> bool {
    let (cx, top, half_w, height) = CAPSULE;
    let bottom = top + height;

    // The capsule: a stadium, so the ends are round rather than chopped.
    if y >= top && y <= bottom && (x - cx).abs() <= half_w {
        let radius = half_w;
        if y >= top + radius && y <= bottom - radius {
            return true;
        }
        let centre_y = if y < top + radius {
            top + radius
        } else {
            bottom - radius
        };
        let dy = y - centre_y;
        if (x - cx).powi(2) + dy.powi(2) <= radius * radius {
            return true;
        }
    }

    false
}

/// The rest of the silhouette, drawn dim: cradle, stem, base.
fn in_stand(x: f32, y: f32) -> bool {
    // The cradle: the lower half of a ring around the capsule's lower end.
    let (arc_x, arc_y, arc_r, thickness) = CRADLE;
    let distance = ((x - arc_x).powi(2) + (y - arc_y).powi(2)).sqrt();
    if y >= arc_y && (distance - arc_r).abs() <= thickness / 2.0 {
        return true;
    }

    let (sx, sy, sw, sh) = STEM;
    if x >= sx && x <= sx + sw && y >= sy && y <= sy + sh {
        return true;
    }
    let (bx, by, bw, bh) = BASE;
    x >= bx && x <= bx + bw && y >= by && y <= by + bh
}

fn corner_alpha(x: f32, y: f32, width: u32, height: u32, radius: f32) -> u8 {
    let (w, h) = (width as f32, height as f32);
    let (cx, cy) = if x < radius && y < radius {
        (radius, radius)
    } else if x > w - radius && y < radius {
        (w - radius, radius)
    } else if x < radius && y > h - radius {
        (radius, h - radius)
    } else if x > w - radius && y > h - radius {
        (w - radius, h - radius)
    } else {
        return 255;
    };
    let distance = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    if distance <= radius - 1.0 {
        255
    } else if distance >= radius {
        0
    } else {
        ((radius - distance) * 255.0) as u8
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _transform: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _output: &wayland_client::protocol::wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _output: &wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _output: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _output: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _output: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl LayerShellHandler for App {
    fn closed(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _layer: &LayerSurface,
    ) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _connection: &wayland_client::Connection,
        _queue: &wayland_client::QueueHandle<Self>,
        _layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.configured = true;
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);
