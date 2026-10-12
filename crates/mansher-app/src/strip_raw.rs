//! Raw-Wayland hover sensor: no GTK, no C shims, every protocol byte ours.
//!
//! A `width`x6 layer-shell surface (Top, exclusive 0) with an explicit
//! full-rect input region and one transparent SHM buffer. Pointer
//! enter/leave go over the app's Msg channel; the tick maps them onto the
//! same HotEnter/HotLeave + zone logic the GTK strip used.
//!
//! Why raw: gtk-layer-shell snapshots the surface size at map time and
//! stuck our GTK sensor at the empty-window default (200px) forever —
//! configured, acked, mapped, but permanently bufferless and eventless
//! (proven in WAYLAND_DEBUG). Here size, configure-ack, buffer attach and
//! input region are all explicit.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::mpsc;

use anyhow::{Context, Result};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
        wl_shm_pool::WlShmPool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::Msg;

const STRIP_H: i32 = 6;

pub fn start(tx: mpsc::Sender<Msg>, width: i32) {
    std::thread::spawn(move || {
        if let Err(e) = run(tx, width) {
            eprintln!("mansher: raw strip unavailable: {e:#}");
        }
    });
}

struct Strip {
    tx: mpsc::Sender<Msg>,
    width: i32,
    globals: Vec<(u32, String, u32)>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    attached: bool,
    pointer: Option<wl_pointer::WlPointer>,
    running: bool,
}

fn find(globals: &[(u32, String, u32)], interface: &str) -> Option<(u32, u32)> {
    globals
        .iter()
        .find(|(_, iface, _)| iface == interface)
        .map(|(name, _, version)| (*name, *version))
}

fn run(tx: mpsc::Sender<Msg>, width: i32) -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland connection")?;
    let mut queue = conn.new_event_queue::<Strip>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());

    let mut st = Strip {
        tx,
        width,
        globals: Vec::new(),
        compositor: None,
        shm: None,
        shell: None,
        seat: None,
        surface: None,
        layer: None,
        buffer: None,
        attached: false,
        pointer: None,
        running: true,
    };
    queue.roundtrip(&mut st).context("registry roundtrip")?;

    let (name, ver) = find(&st.globals, "wl_compositor").context("no wl_compositor")?;
    st.compositor =
        Some(registry.bind::<wl_compositor::WlCompositor, (), Strip>(name, ver.min(6), &qh, ()));
    let (name, ver) = find(&st.globals, "wl_shm").context("no wl_shm")?;
    st.shm = Some(registry.bind::<wl_shm::WlShm, (), Strip>(name, ver.min(1), &qh, ()));
    let (name, ver) = find(&st.globals, "zwlr_layer_shell_v1").context("no wlr-layer-shell")?;
    st.shell = Some(registry.bind::<ZwlrLayerShellV1, (), Strip>(name, ver.min(4), &qh, ()));
    let (name, ver) = find(&st.globals, "wl_seat").context("no wl_seat")?;
    st.seat = Some(registry.bind::<wl_seat::WlSeat, (), Strip>(name, ver.min(7), &qh, ()));
    queue.roundtrip(&mut st).context("bind roundtrip")?;

    let compositor = st.compositor.clone().context("compositor bind failed")?;
    let surface = compositor.create_surface(&qh, ());
    // Explicit full-rect input region: a transparent surface must still
    // be reactive everywhere. The compositor copies the region on set.
    let region = compositor.create_region(&qh, ());
    region.add(0, 0, width, STRIP_H);
    surface.set_input_region(Some(&region));

    let shell = st.shell.clone().context("shell bind failed")?;
    let layer =
        shell.get_layer_surface(&surface, None, Layer::Top, "mansher-strip".into(), &qh, ());
    layer.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Exact size, stated by us: width stretches via L+R anchors, height 6.
    layer.set_size(0, STRIP_H as u32);
    // Empty initial commit (no buffer attached — that must wait for the
    // configure ack). Without this kick the compositor never configures.
    surface.commit();

    // One transparent SHM buffer: a surface with no committed buffer gets
    // no input in Niri, so a blank-but-present buffer is required. Backed
    // by memfd (anonymous RAM, no filesystem involved): plain shm files in
    // $XDG_RUNTIME_DIR produced "Failed to mmap fd" from the compositor
    // here despite being present, sized and mmap-able client-side.
    let stride = width * 4;
    let len = (stride * STRIP_H) as usize;
    let fd: OwnedFd = unsafe {
        let raw = libc::memfd_create(c"mansher-strip".as_ptr(), libc::MFD_CLOEXEC);
        if raw < 0 {
            anyhow::bail!("memfd_create failed");
        }
        if libc::ftruncate(raw, len as libc::off_t) != 0 {
            anyhow::bail!("memfd ftruncate failed");
        }
        OwnedFd::from_raw_fd(raw)
    };
    let pool =
        st.shm
            .clone()
            .context("shm bind failed")?
            .create_pool(fd.as_fd(), len as i32, &qh, ());
    let buffer = pool.create_buffer(0, width, STRIP_H, stride, wl_shm::Format::Argb8888, &qh, ());
    // Buffer is attached on the first configure, never before: the
    // protocol forbids attaching prior to acking the initial configure.
    st.surface = Some(surface);
    st.buffer = Some(buffer);
    st.layer = Some(layer);

    maybe_pointer(&mut st, &qh);

    while st.running {
        queue.blocking_dispatch(&mut st)?;
    }
    Ok(())
}

/// Create the pointer object once the seat offers the capability (caps may
/// arrive during either roundtrip, or later on hot-plugged seats).
fn maybe_pointer(st: &mut Strip, qh: &QueueHandle<Strip>) {
    if st.pointer.is_some() {
        return;
    }
    if let Some(seat) = st.seat.clone() {
        st.pointer = Some(seat.get_pointer(qh, ()));
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Strip {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Strip>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Strip {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Strip>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) {
                maybe_pointer(state, qh);
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Strip {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Strip>,
    ) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => {
                if Some(&surface) == state.surface.as_ref() {
                    let _ = state.tx.send(Msg::StripEnter);
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if Some(&surface) == state.surface.as_ref() {
                    let _ = state.tx.send(Msg::StripLeave);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Strip {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Strip>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Strip {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Strip>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Strip {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        event: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Strip>,
    ) {
        let _ = event;
        unreachable!("zwlr_layer_shell_v1 sends no events");
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Strip {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Strip>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                if let Some(surface) = state.surface.as_ref() {
                    if !state.attached {
                        if let Some(buffer) = state.buffer.as_ref() {
                            surface.attach(Some(buffer), 0, 0);
                            surface.damage(0, 0, state.width, STRIP_H);
                        }
                        state.attached = true;
                    }
                    surface.commit();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                eprintln!("mansher: strip surface closed");
                state.running = false;
            }
            _ => {}
        }
    }
}

delegate_noop!(Strip: ignore wl_compositor::WlCompositor);
delegate_noop!(Strip: ignore wl_surface::WlSurface);
delegate_noop!(Strip: ignore WlShmPool);
delegate_noop!(Strip: ignore wl_region::WlRegion);
