mod compositor;
mod data_device;
mod decoration;
pub mod panel_item;
mod seat;
mod shaders;
mod state;
mod surface;
// mod xdg_activation;
mod xdg_shell;

use self::{state::WaylandState, surface::CORE_SURFACES};
use crate::{core::task, wayland::state::ClientState};
use color_eyre::eyre::{ensure, Result};
use global_counter::primitive::exact::CounterU32;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use sk::lifecycle::StereoKitDraw;
use smithay::backend::egl::EGLContext;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::wayland_server::{backend::GlobalId, Display, ListeningSocket};
use std::os::unix::prelude::AsRawFd;
use std::{
	ffi::c_void,
	os::unix::{net::UnixListener, prelude::FromRawFd},
	sync::Arc,
};
use stereokit as sk;
use tokio::{
	io::unix::AsyncFd, net::UnixListener as AsyncUnixListener, sync::mpsc, task::JoinHandle,
};
use tracing::{debug, debug_span, info, instrument};

pub static SERIAL_COUNTER: CounterU32 = CounterU32::new(0);

struct EGLRawHandles {
	display: *const c_void,
	config: *const c_void,
	context: *const c_void,
}
fn get_sk_egl() -> Result<EGLRawHandles> {
	ensure!(
		unsafe { sk::sys::backend_graphics_get() }
			== sk::sys::backend_graphics__backend_graphics_opengles_egl,
		"StereoKit is not running using EGL!"
	);

	Ok(unsafe {
		EGLRawHandles {
			display: sk::sys::backend_opengl_egl_get_display() as *const c_void,
			config: sk::sys::backend_opengl_egl_get_config() as *const c_void,
			context: sk::sys::backend_opengl_egl_get_context() as *const c_void,
		}
	})
}

static GLOBAL_DESTROY_QUEUE: OnceCell<mpsc::Sender<GlobalId>> = OnceCell::new();

pub struct Wayland {
	display: Arc<Mutex<Display<WaylandState>>>,
	pub socket_name: String,
	join_handle: JoinHandle<Result<()>>,
	renderer: GlesRenderer,
	state: Arc<Mutex<WaylandState>>,
}
impl Wayland {
	pub fn new() -> Result<Self> {
		let egl_raw_handles = get_sk_egl()?;
		let renderer = unsafe {
			GlesRenderer::new(EGLContext::from_raw(
				egl_raw_handles.display,
				egl_raw_handles.config,
				egl_raw_handles.context,
			)?)?
		};

		let display: Display<WaylandState> = Display::new()?;
		let display_handle = display.handle();

		let display = Arc::new(Mutex::new(display));
		let state = WaylandState::new(display.clone(), display_handle, &renderer);

		let (global_destroy_queue_in, global_destroy_queue) = mpsc::channel(8);
		GLOBAL_DESTROY_QUEUE.set(global_destroy_queue_in).unwrap();

		let socket = ListeningSocket::bind_auto("wayland", 0..33)?;
		let socket_name = socket.socket_name().unwrap().to_str().unwrap().to_string();
		info!(socket_name, "Wayland active");

		let join_handle =
			Wayland::start_loop(display.clone(), socket, state.clone(), global_destroy_queue)?;

		Ok(Wayland {
			display,
			socket_name,
			join_handle,
			renderer,
			state,
		})
	}

	fn start_loop(
		display: Arc<Mutex<Display<WaylandState>>>,
		socket: ListeningSocket,
		state: Arc<Mutex<WaylandState>>,
		mut global_destroy_queue: mpsc::Receiver<GlobalId>,
	) -> Result<JoinHandle<Result<()>>> {
		let listen_async =
			AsyncUnixListener::from_std(unsafe { UnixListener::from_raw_fd(socket.as_raw_fd()) })?;

		let dispatch_poll_fd = display.lock().backend().poll_fd().try_clone_to_owned()?;
		let dispatch_poll_listener = AsyncFd::new(dispatch_poll_fd)?;

		let dh1 = display.lock().handle();
		let mut dh2 = dh1.clone();

		Ok(task::new(|| "wayland loop", async move {
			let _socket = socket; // Keep the socket alive
			loop {
				tokio::select! {
					e = global_destroy_queue.recv() => { // New global to destroy
						debug!(?e, "destroy global");
						dh1.remove_global::<WaylandState>(e.unwrap());
					}
					acc = listen_async.accept() => { // New client connected
						let (stream, _) = acc?;
						let client = dh2.insert_client(stream.into_std()?, Arc::new(ClientState))?;

						state.lock().new_client(client.id(), &dh2);
					}
					e = dispatch_poll_listener.readable() => { // Dispatch
						let mut guard = e?;
						debug_span!("Dispatch wayland event").in_scope(|| -> Result<(), color_eyre::Report> {
							let mut display = display.lock();
							display.dispatch_clients(&mut *state.lock())?;
							display.flush_clients()?;
							Ok(())
						})?;
						guard.clear_ready();
					}
				}
			}
		})?)
	}

	#[instrument(level = "debug", name = "Wayland frame", skip(self, sk))]
	pub fn update(&mut self, sk: &StereoKitDraw) {
		for core_surface in CORE_SURFACES.get_valid_contents() {
			core_surface.process(sk, &mut self.renderer);
		}

		self.display.lock().flush_clients().unwrap();
	}

	pub fn frame_event(&self, sk: &StereoKitDraw) {
		let state = self.state.lock();

		for core_surface in CORE_SURFACES.get_valid_contents() {
			core_surface.frame(sk, state.output.clone());
		}
	}

	pub fn make_context_current(&self) {
		unsafe {
			self.renderer.egl_context().make_current().unwrap();
		}
	}
}
impl Drop for Wayland {
	fn drop(&mut self) {
		self.join_handle.abort();
	}
}
