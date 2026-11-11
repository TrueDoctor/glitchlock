use std::collections::HashMap;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use wayland_client::backend::WaylandError;
use wayland_client::globals::{BindError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::ExtSessionLockManagerV1;
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_surface_v1::{
    self, ExtSessionLockSurfaceV1,
};
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_v1::{
    self, ExtSessionLockV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::authenticator::{AuthenticationEvent, Authenticator};
use crate::config::Config;
use crate::glitch::{self, Glitch};
use crate::keyboard::{KeyAction, Keyboard};
use crate::secret::Secret;
use crate::shm::ShmBuffer;
use crate::wallpaper::{self, Wallpaper};

/// Fractional scales are sent in 120ths.
const SCALE_DENOMINATOR: u32 = 120;
const BUFFERS_PER_SURFACE: usize = 2;

/// Locks the session and returns once the user is authenticated or the compositor ends the lock.
pub fn run(config: Config) -> Result<(), String> {
    let wallpaper = config.image.as_deref().and_then(|path| {
        Wallpaper::load_png(path)
            .inspect_err(|error| eprintln!("glitchlock: using the background color, {error}"))
            .ok()
    });

    let connection = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to Wayland: {error}"))?;
    let (globals, mut queue) = registry_queue_init::<App>(&connection)
        .map_err(|error| format!("cannot list Wayland globals: {error}"))?;
    let queue_handle = queue.handle();

    let missing = |name: &'static str| {
        move |error: BindError| format!("the compositor does not offer {name}: {error}")
    };
    let compositor: WlCompositor = globals
        .bind(&queue_handle, 4..=6, ())
        .map_err(missing("wl_compositor"))?;
    let shm: WlShm = globals
        .bind(&queue_handle, 1..=1, ())
        .map_err(missing("wl_shm"))?;
    let lock_manager: ExtSessionLockManagerV1 = globals
        .bind(&queue_handle, 1..=1, ())
        .map_err(missing("ext_session_lock_manager_v1"))?;
    let viewporter: Option<WpViewporter> = globals.bind(&queue_handle, 1..=1, ()).ok();
    let fractional_scale_manager: Option<WpFractionalScaleManagerV1> =
        globals.bind(&queue_handle, 1..=1, ()).ok();

    let authenticator = Authenticator::start(&config.pam_services)?;
    let session_lock = lock_manager.lock(&queue_handle, ());

    let mut app = App {
        queue_handle,
        compositor,
        shm,
        viewporter,
        fractional_scale_manager,
        session_lock,
        lock_state: LockState::Pending,
        unlock_requested: false,
        outputs: HashMap::new(),
        seats: HashMap::new(),
        keyboard: Keyboard::default(),
        password: Secret::new(),
        glitch: Glitch::new(random_seed(), config.hold_times),
        wallpaper,
        color: config.color,
        authenticator,
    };

    for global in globals.contents().clone_list() {
        app.add_global(
            globals.registry(),
            global.name,
            &global.interface,
            global.version,
        );
    }
    app.run_event_loop(&connection, &mut queue)
}

struct App {
    queue_handle: QueueHandle<App>,
    compositor: WlCompositor,
    shm: WlShm,
    viewporter: Option<WpViewporter>,
    fractional_scale_manager: Option<WpFractionalScaleManagerV1>,
    session_lock: ExtSessionLockV1,
    lock_state: LockState,
    unlock_requested: bool,
    /// Outputs by their global name.
    outputs: HashMap<u32, Output>,
    /// Seats by their global name.
    seats: HashMap<u32, Seat>,
    keyboard: Keyboard,
    password: Secret,
    glitch: Glitch,
    wallpaper: Option<Wallpaper>,
    color: u32,
    authenticator: Authenticator,
}

#[derive(Clone, Copy, PartialEq)]
enum LockState {
    Pending,
    Locked,
    Finished { was_locked: bool },
}

struct Output {
    wl_output: WlOutput,
    surface: LockSurface,
}

struct Seat {
    wl_seat: WlSeat,
    keyboard: Option<WlKeyboard>,
}

struct LockSurface {
    wl_surface: WlSurface,
    lock_surface: ExtSessionLockSurfaceV1,
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    logical_size: Option<(u32, u32)>,
    scale: u32,
    buffer_size: (usize, usize),
    background: Vec<u32>,
    buffers: Vec<ShmBuffer>,
    /// Increments whenever `buffers` is replaced, so releases of old buffers are ignored.
    generation: u32,
    /// Rows that differ from the background in the buffer the compositor shows.
    shown_dirty_rows: Vec<Range<usize>>,
    redraw_pending: bool,
}

#[derive(Clone, Copy)]
struct BufferKey {
    output: u32,
    generation: u32,
    index: usize,
}

impl App {
    fn run_event_loop(
        &mut self,
        connection: &Connection,
        queue: &mut EventQueue<App>,
    ) -> Result<(), String> {
        loop {
            queue
                .dispatch_pending(self)
                .map_err(|error| format!("Wayland dispatch failed: {error}"))?;

            match self.lock_state {
                LockState::Finished { was_locked: false } => {
                    return Err(String::from("the compositor refused to lock the session"));
                }
                LockState::Finished { was_locked: true } => return Ok(()),
                LockState::Locked if self.unlock_requested => {
                    self.session_lock.unlock_and_destroy();
                    queue
                        .roundtrip(self)
                        .map_err(|error| format!("cannot confirm the unlock: {error}"))?;
                    return Ok(());
                }
                _ => {}
            }

            match connection.flush() {
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                result => result.map_err(|error| format!("cannot write to Wayland: {error}"))?,
            }

            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let helper_fds = self.authenticator.output_fds();
            let mut poll_fds: Vec<libc::pollfd> =
                std::iter::once(guard.connection_fd().as_raw_fd())
                    .chain(helper_fds.iter().map(|(_, fd)| *fd))
                    .map(|fd| libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    })
                    .collect();

            if !poll(&mut poll_fds, self.poll_timeout())? {
                continue;
            }

            let wayland_ready = poll_fds.first().is_some_and(|poll_fd| poll_fd.revents != 0);
            if wayland_ready {
                match guard.read() {
                    Err(WaylandError::Io(error))
                        if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    result => {
                        result.map_err(|error| format!("cannot read from Wayland: {error}"))?;
                    }
                }
            } else {
                drop(guard);
            }

            let now = Instant::now();
            for ((index, _), poll_fd) in helper_fds.iter().zip(poll_fds.iter().skip(1)) {
                if poll_fd.revents != 0 {
                    for event in self.authenticator.read_output(*index, now) {
                        self.handle_authentication(event);
                    }
                }
            }
            self.authenticator.restart_due_helpers(now);
            if self.glitch.clear_if_due(now) {
                self.redraw_all();
            }
        }
    }

    /// Milliseconds until the next timed event, or -1 to wait indefinitely.
    fn poll_timeout(&self) -> i32 {
        let next_deadline = [self.authenticator.next_restart(), self.glitch.clear_at()]
            .into_iter()
            .flatten()
            .min();
        let Some(deadline) = next_deadline else {
            return -1;
        };
        let milliseconds = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            + 1;
        i32::try_from(milliseconds).unwrap_or(i32::MAX)
    }

    fn add_global(&mut self, registry: &WlRegistry, name: u32, interface: &str, version: u32) {
        if interface == WlOutput::interface().name {
            let wl_output: WlOutput = registry.bind(name, version.min(4), &self.queue_handle, ());
            let surface = self.create_lock_surface(name, &wl_output);
            self.outputs.insert(name, Output { wl_output, surface });
        } else if interface == WlSeat::interface().name {
            let wl_seat: WlSeat = registry.bind(name, version.min(7), &self.queue_handle, name);
            self.seats.insert(
                name,
                Seat {
                    wl_seat,
                    keyboard: None,
                },
            );
        }
    }

    fn create_lock_surface(&self, output_name: u32, wl_output: &WlOutput) -> LockSurface {
        let queue_handle = &self.queue_handle;
        let wl_surface = self.compositor.create_surface(queue_handle, ());
        let lock_surface =
            self.session_lock
                .get_lock_surface(&wl_surface, wl_output, queue_handle, output_name);

        // Fractional scaling needs a viewport to map the larger buffer onto the surface.
        let viewport = self
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&wl_surface, queue_handle, ()));
        let fractional_scale = self
            .fractional_scale_manager
            .as_ref()
            .filter(|_| viewport.is_some())
            .map(|manager| manager.get_fractional_scale(&wl_surface, queue_handle, output_name));

        LockSurface {
            wl_surface,
            lock_surface,
            viewport,
            fractional_scale,
            logical_size: None,
            scale: SCALE_DENOMINATOR,
            buffer_size: (0, 0),
            background: Vec::new(),
            buffers: Vec::new(),
            generation: 0,
            shown_dirty_rows: Vec::new(),
            redraw_pending: false,
        }
    }

    fn handle_key(&mut self, action: KeyAction) {
        let now = Instant::now();
        match action {
            KeyAction::Insert(character) => {
                if self.password.push(character) {
                    self.glitch.advance(now);
                }
            }
            KeyAction::Erase => {
                if self.password.pop() {
                    match self.password.is_empty() {
                        true => self.glitch.clear(),
                        false => self.glitch.advance(now),
                    }
                }
            }
            KeyAction::Clear => {
                self.password.clear();
                self.glitch.cleared(now);
            }
            KeyAction::Submit => {
                if !self.password.is_empty() {
                    self.glitch.verify(now);
                    self.authenticator.submit(self.password.take());
                }
            }
        }
        self.redraw_all();
    }

    fn handle_authentication(&mut self, event: AuthenticationEvent) {
        match event {
            AuthenticationEvent::Unlocked => self.unlock_requested = true,
            AuthenticationEvent::Rejected => {
                self.glitch.reject(Instant::now());
                self.redraw_all();
            }
        }
    }

    fn redraw_all(&mut self) {
        for (name, output) in &mut self.outputs {
            output
                .surface
                .draw(&self.shm, &self.queue_handle, *name, &self.glitch);
        }
    }

    fn redraw(&mut self, output_name: u32) {
        if let Some(output) = self.outputs.get_mut(&output_name) {
            output
                .surface
                .draw(&self.shm, &self.queue_handle, output_name, &self.glitch);
        }
    }

    /// Updates the buffer size after a configure or scale change and redraws.
    fn resize(&mut self, output_name: u32) {
        let Some(output) = self.outputs.get_mut(&output_name) else {
            return;
        };
        output.surface.resize(self.wallpaper.as_ref(), self.color);
        output
            .surface
            .draw(&self.shm, &self.queue_handle, output_name, &self.glitch);
    }
}

impl LockSurface {
    fn resize(&mut self, wallpaper: Option<&Wallpaper>, color: u32) {
        let Some((width, height)) = self.logical_size else {
            return;
        };
        if let Some(viewport) = &self.viewport
            && let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height))
        {
            viewport.set_destination(width, height);
        }

        let scaled = |length: u32| {
            (u64::from(length) * u64::from(self.scale)).div_ceil(u64::from(SCALE_DENOMINATOR))
        };
        let buffer_size = (scaled(width) as usize, scaled(height) as usize);
        if buffer_size == self.buffer_size {
            return;
        }

        let (buffer_width, buffer_height) = buffer_size;
        self.buffer_size = buffer_size;
        self.background = match wallpaper {
            Some(wallpaper) => wallpaper.cover(buffer_width, buffer_height),
            None => wallpaper::solid(color, buffer_width, buffer_height),
        };
        self.buffers.clear();
        self.generation = self.generation.wrapping_add(1);
        self.shown_dirty_rows.clear();
    }

    fn draw(
        &mut self,
        shm: &WlShm,
        queue_handle: &QueueHandle<App>,
        output_name: u32,
        glitch: &Glitch,
    ) {
        let (width, height) = self.buffer_size;
        if width == 0 || height == 0 {
            return;
        }
        let Some(index) = self.free_buffer(shm, queue_handle, output_name) else {
            self.redraw_pending = true;
            return;
        };
        let Some(buffer) = self.buffers.get_mut(index) else {
            return;
        };

        let effects = glitch.effects(&self.background, width);
        let restored_rows = std::mem::take(&mut buffer.dirty_rows);
        let pixels = buffer.pixels_mut();
        for rows in &restored_rows {
            glitch::restore_rows(&self.background, pixels, width, rows.clone());
        }
        for effect in &effects {
            effect.draw(&self.background, pixels, width);
        }
        buffer.dirty_rows = effects.iter().map(|effect| effect.rows(height)).collect();

        let changed_rows = restored_rows
            .iter()
            .chain(&self.shown_dirty_rows)
            .chain(&buffer.dirty_rows);
        for rows in changed_rows {
            if let (Ok(top), Ok(count), Ok(width)) = (
                i32::try_from(rows.start),
                i32::try_from(rows.len()),
                i32::try_from(width),
            ) {
                self.wl_surface.damage_buffer(0, top, width, count);
            }
        }

        self.shown_dirty_rows = buffer.dirty_rows.clone();
        buffer.busy = true;
        self.wl_surface.attach(Some(&buffer.wl_buffer), 0, 0);
        self.wl_surface.commit();
        self.redraw_pending = false;
    }

    /// The index of a buffer the compositor is not reading from, creating one if needed.
    fn free_buffer(
        &mut self,
        shm: &WlShm,
        queue_handle: &QueueHandle<App>,
        output_name: u32,
    ) -> Option<usize> {
        if let Some(index) = self.buffers.iter().position(|buffer| !buffer.busy) {
            return Some(index);
        }
        if self.buffers.len() >= BUFFERS_PER_SURFACE {
            return None;
        }

        let (width, height) = self.buffer_size;
        let key = BufferKey {
            output: output_name,
            generation: self.generation,
            index: self.buffers.len(),
        };
        let mut buffer = ShmBuffer::new(shm, width, height, queue_handle, key)
            .inspect_err(|error| eprintln!("glitchlock: cannot create a buffer: {error}"))
            .ok()?;
        // A new buffer holds zeroes, so every row has to be restored from the background.
        buffer.dirty_rows.push(0..height);
        self.buffers.push(buffer);
        Some(key.index)
    }
}

impl Drop for LockSurface {
    fn drop(&mut self) {
        self.buffers.clear();
        if let Some(fractional_scale) = &self.fractional_scale {
            fractional_scale.destroy();
        }
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
        self.lock_surface.destroy();
        self.wl_surface.destroy();
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if self.wl_output.version() >= 3 {
            self.wl_output.release();
        }
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        if let Some(keyboard) = &self.keyboard {
            keyboard.release();
        }
        self.wl_seat.release();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for App {
    fn event(
        app: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => app.add_global(registry, name, &interface, version),
            wl_registry::Event::GlobalRemove { name } => {
                app.outputs.remove(&name);
                app.seats.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => app.lock_state = LockState::Locked,
            ext_session_lock_v1::Event::Finished => {
                app.lock_state = LockState::Finished {
                    was_locked: app.lock_state == LockState::Locked,
                };
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, u32> for App {
    fn event(
        app: &mut Self,
        lock_surface: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        output_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
            && let Some(output) = app.outputs.get_mut(output_name)
        {
            lock_surface.ack_configure(serial);
            output.surface.logical_size = Some((width, height));
            app.resize(*output_name);
        }
    }
}

impl Dispatch<WpFractionalScaleV1, u32> for App {
    fn event(
        app: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        output_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(output) = app.outputs.get_mut(output_name)
        {
            output.surface.scale = scale.max(1);
            app.resize(*output_name);
        }
    }
}

impl Dispatch<WlBuffer, BufferKey> for App {
    fn event(
        app: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        key: &BufferKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_buffer::Event::Release = event else {
            return;
        };
        let Some(output) = app.outputs.get_mut(&key.output) else {
            return;
        };
        let surface = &mut output.surface;
        if surface.generation != key.generation {
            return;
        }
        if let Some(buffer) = surface.buffers.get_mut(key.index) {
            buffer.busy = false;
        }
        if surface.redraw_pending {
            app.redraw(key.output);
        }
    }
}

impl Dispatch<WlSeat, u32> for App {
    fn event(
        app: &mut Self,
        wl_seat: &WlSeat,
        event: wl_seat::Event,
        seat_name: &u32,
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        let Some(seat) = app.seats.get_mut(seat_name) else {
            return;
        };

        let has_keyboard = capabilities.contains(Capability::Keyboard);
        match (&seat.keyboard, has_keyboard) {
            (None, true) => seat.keyboard = Some(wl_seat.get_keyboard(queue_handle, ())),
            (Some(keyboard), false) => {
                keyboard.release();
                seat.keyboard = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(KeymapFormat::XkbV1),
                fd,
                size,
            } => app.keyboard.load_keymap(fd, size),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => app
                .keyboard
                .set_modifiers(mods_depressed, mods_latched, mods_locked, group),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(KeyState::Pressed),
                ..
            } => {
                if let Some(action) = app.keyboard.press(key) {
                    app.handle_key(action);
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(App: WlCompositor);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore WlOutput);
delegate_noop!(App: WpViewporter);
delegate_noop!(App: WpViewport);
delegate_noop!(App: WpFractionalScaleManagerV1);
delegate_noop!(App: ExtSessionLockManagerV1);

/// Waits for any file descriptor to become readable, returning `false` when interrupted.
fn poll(poll_fds: &mut [libc::pollfd], timeout: i32) -> Result<bool, String> {
    let count = libc::nfds_t::try_from(poll_fds.len()).map_err(|error| error.to_string())?;
    // SAFETY: `poll_fds` is a valid array of `count` entries.
    if unsafe { libc::poll(poll_fds.as_mut_ptr(), count, timeout) } >= 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();
    match error.kind() {
        std::io::ErrorKind::Interrupted => Ok(false),
        _ => Err(format!("poll failed: {error}")),
    }
}

fn random_seed() -> u64 {
    let nanoseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    nanoseconds ^ u64::from(std::process::id()).rotate_left(32)
}
