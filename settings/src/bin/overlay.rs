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

/// Bars in the meter. Matches the Windows overlay and the daemon's band count.
const BARS: usize = 36;
/// The panel, in logical pixels.
const WIDTH: u32 = 236;
const HEIGHT: u32 = 58;
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
    bands: Vec<f32>,
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

        paint(canvas, WIDTH, HEIGHT, &self.tick);

        buffer.attach_to(self.layer.wl_surface()).ok();
        self.layer.commit();
    }
}

/// Draw the panel: rounded dark background, a status dot, and a symmetric meter.
///
/// The buffer is ARGB8888 in native byte order, premultiplied. Only the corner
/// pixels need alpha, so the rounding is a distance test rather than a mask.
fn paint(canvas: &mut [u8], width: u32, height: u32, tick: &Tick) {
    let radius = 10.0f32;
    let mut canvas = Canvas {
        pixels: canvas,
        width,
        height,
    };

    canvas.rounded_panel(radius);
    // Green while waiting for a phrase, blue while streaming.
    let dot = if tick.streaming {
        (91, 155, 213)
    } else {
        (76, 175, 122)
    };
    canvas.circle(18.0, 20.0, 5.0, dot);

    // A symmetric meter, so it reads as a waveform rather than a level bar: each
    // column grows above and below the centre line.
    let gap = 2i32;
    let bar = ((width as i32 - 36 - gap * (BARS as i32 - 1)) / BARS as i32).max(2);
    let total = bar * BARS as i32 + gap * (BARS as i32 - 1);
    let left = (width as i32 - total) / 2;
    let top = 34;
    let bottom = height as i32 - 10;
    let centre = (top + bottom) / 2;
    let reach = ((bottom - top) / 2 - 2).max(2) as f32;

    for index in 0..BARS {
        let value = tick
            .bands
            .get(index)
            .copied()
            .unwrap_or(if index == 0 { tick.level } else { 0.0 })
            .clamp(0.0, 1.0);
        let half = (value * reach).max(1.0);
        let x = left + index as i32 * (bar + gap);
        // Brighter with amplitude, so the meter reads at a glance.
        let colour = (
            (60.0 + 120.0 * value) as u8,
            (130.0 + 90.0 * value) as u8,
            (200.0 - 30.0 * value) as u8,
        );
        canvas.rect(
            x,
            (centre as f32 - half) as i32,
            bar,
            (half * 2.0) as i32,
            colour,
        );
    }
}

/// The pixel buffer, with the two shapes this panel needs.
struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
}

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

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, colour: (u8, u8, u8)) {
        let (width, height) = (self.width as i32, self.height as i32);
        for row in y.max(0)..(y + h).min(height) {
            for column in x.max(0)..(x + w).min(width) {
                let offset = ((row as u32 * self.width + column as u32) * 4) as usize;
                self.pixels[offset] = colour.2;
                self.pixels[offset + 1] = colour.1;
                self.pixels[offset + 2] = colour.0;
                self.pixels[offset + 3] = 255;
            }
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, radius: f32, colour: (u8, u8, u8)) {
        let (width, height) = (self.width as i32, self.height as i32);
        let (x0, x1) = ((cx - radius) as i32, (cx + radius).ceil() as i32);
        let (y0, y1) = ((cy - radius) as i32, (cy + radius).ceil() as i32);
        for row in y0.max(0)..y1.min(height) {
            for column in x0.max(0)..x1.min(width) {
                let dx = column as f32 + 0.5 - cx;
                let dy = row as f32 + 0.5 - cy;
                if dx * dx + dy * dy <= radius * radius {
                    let offset = ((row as u32 * self.width + column as u32) * 4) as usize;
                    self.pixels[offset] = colour.2;
                    self.pixels[offset + 1] = colour.1;
                    self.pixels[offset + 2] = colour.0;
                    self.pixels[offset + 3] = 255;
                }
            }
        }
    }
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
