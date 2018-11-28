use std::{
    cell::RefCell,
    collections::HashMap,
    io::Error as IoError,
    os::unix::io::{AsRawFd, RawFd},
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use glium::Surface as GliumSurface;
use slog::Logger;

#[cfg(feature = "egl")]
use smithay::backend::egl::{EGLDisplay, EGLGraphicsBackend};
use smithay::{
    backend::{
        drm::{
            dev_t, device_bind,
            egl::{EglDevice, EglSurface},
            gbm::{egl::Gbm as EglGbmBackend, GbmDevice},
            legacy::LegacyDrmDevice,
            DevPath, Device, DeviceHandler, Surface,
        },
        graphics::CursorBackend,
        input::InputBackend,
        libinput::{libinput_bind, LibinputInputBackend, LibinputSessionInterface},
        session::{
            auto::{auto_session_bind, AutoSession},
            notify_multiplexer, AsSessionObserver, OFlag, Session, SessionNotifier,
        },
        udev::{primary_gpu, udev_backend_bind, UdevBackend, UdevHandler},
    },
    drm::control::{
        connector::{Info as ConnectorInfo, State as ConnectorState},
        crtc,
        encoder::Info as EncoderInfo,
        ResourceInfo,
    },
    image::{ImageBuffer, Rgba},
    input::Libinput,
    wayland::{
        compositor::CompositorToken,
        data_device::{default_action_chooser, init_data_device, set_data_device_focus},
        output::{Mode, Output, PhysicalProperties},
        seat::{Seat, XkbConfig},
        shm::init_shm_global,
    },
    wayland_server::{
        calloop::{
            generic::{EventedFd, Generic},
            EventLoop, LoopHandle, Source,
        },
        protocol::wl_output,
        Display,
    },
};

use glium_drawer::GliumDrawer;
use input_handler::AnvilInputHandler;
use shell::{init_shell, MyWindowMap, Roles, SurfaceData};

pub struct SessionFd(RawFd);
impl AsRawFd for SessionFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

type RenderDevice =
    EglDevice<EglGbmBackend<LegacyDrmDevice<SessionFd>>, GbmDevice<LegacyDrmDevice<SessionFd>>>;
type RenderSurface =
    EglSurface<EglGbmBackend<LegacyDrmDevice<SessionFd>>, GbmDevice<LegacyDrmDevice<SessionFd>>>;

pub fn run_udev(mut display: Display, mut event_loop: EventLoop<()>, log: Logger) -> Result<(), ()> {
    let name = display.add_socket_auto().unwrap().into_string().unwrap();
    info!(log, "Listening on wayland socket"; "name" => name.clone());
    ::std::env::set_var("WAYLAND_DISPLAY", name);

    #[cfg(feature = "egl")]
    let active_egl_context = Rc::new(RefCell::new(None));

    let display = Rc::new(RefCell::new(display));

    /*
     * Initialize the compositor
     */
    init_shm_global(&mut display.borrow_mut(), vec![], log.clone());

    let (compositor_token, _, _, window_map) = init_shell(&mut display.borrow_mut(), log.clone());

    /*
     * Initialize session
     */
    let (session, mut notifier) = AutoSession::new(log.clone()).ok_or(())?;
    let (udev_observer, udev_notifier) = notify_multiplexer();
    let udev_session_id = notifier.register(udev_observer);

    let running = Arc::new(AtomicBool::new(true));

    let pointer_location = Rc::new(RefCell::new((0.0, 0.0)));

    /*
     * Initialize the udev backend
     */
    let context = ::smithay::udev::Context::new().map_err(|_| ())?;
    let seat = session.seat();

    let primary_gpu = primary_gpu(&context, &seat).unwrap_or_default();

    let bytes = include_bytes!("../resources/cursor2.rgba");
    let udev_backend = UdevBackend::new(
        &context,
        UdevHandlerImpl {
            compositor_token,
            #[cfg(feature = "egl")]
            active_egl_context,
            session: session.clone(),
            backends: HashMap::new(),
            display: display.clone(),
            primary_gpu,
            window_map: window_map.clone(),
            pointer_location: pointer_location.clone(),
            pointer_image: ImageBuffer::from_raw(64, 64, bytes.to_vec()).unwrap(),
            loop_handle: event_loop.handle(),
            notifier: udev_notifier,
            logger: log.clone(),
        },
        seat.clone(),
        log.clone(),
    ).map_err(|_| ())?;

    init_data_device(
        &mut display.borrow_mut(),
        |_| {},
        default_action_chooser,
        log.clone(),
    );

    let (mut w_seat, _) = Seat::new(&mut display.borrow_mut(), session.seat(), log.clone());

    let pointer = w_seat.add_pointer();
    let keyboard = w_seat
        .add_keyboard(XkbConfig::default(), 1000, 500, |seat, focus| {
            set_data_device_focus(seat, focus.and_then(|s| s.client()))
        }).expect("Failed to initialize the keyboard");

    let (output, _output_global) = Output::new(
        &mut display.borrow_mut(),
        "Drm".into(),
        PhysicalProperties {
            width: 0,
            height: 0,
            subpixel: wl_output::Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Generic DRM".into(),
        },
        log.clone(),
    );

    let (w, h) = (1920, 1080); // Hardcode full-hd res
    output.change_current_state(
        Some(Mode {
            width: w as i32,
            height: h as i32,
            refresh: 60_000,
        }),
        None,
        None,
    );
    output.set_preferred(Mode {
        width: w as i32,
        height: h as i32,
        refresh: 60_000,
    });

    /*
     * Initialize libinput backend
     */
    let mut libinput_context =
        Libinput::new_from_udev::<LibinputSessionInterface<AutoSession>>(session.clone().into(), &context);
    let libinput_session_id = notifier.register(libinput_context.observer());
    libinput_context.udev_assign_seat(&seat).unwrap();
    let mut libinput_backend = LibinputInputBackend::new(libinput_context, log.clone());
    libinput_backend.set_handler(AnvilInputHandler::new_with_session(
        log.clone(),
        pointer,
        keyboard,
        window_map.clone(),
        (w, h),
        running.clone(),
        pointer_location,
        session,
    ));
    let libinput_event_source = libinput_bind(libinput_backend, event_loop.handle())
        .map_err(|e| -> IoError { e.into() })
        .unwrap();
    let session_event_source = auto_session_bind(notifier, &event_loop.handle())
        .map_err(|(e, _)| e)
        .unwrap();
    let udev_event_source = udev_backend_bind(udev_backend, &event_loop.handle())
        .map_err(|e| -> IoError { e.into() })
        .unwrap();

    while running.load(Ordering::SeqCst) {
        if event_loop
            .dispatch(Some(::std::time::Duration::from_millis(16)), &mut ())
            .is_err()
        {
            running.store(false, Ordering::SeqCst);
        } else {
            display.borrow_mut().flush_clients();
            window_map.borrow_mut().refresh();
        }
    }

    let mut notifier = session_event_source.unbind();
    notifier.unregister(libinput_session_id);
    notifier.unregister(udev_session_id);

    libinput_event_source.remove();
    udev_event_source.remove();

    Ok(())
}

struct UdevHandlerImpl<S: SessionNotifier, Data: 'static> {
    compositor_token: CompositorToken<SurfaceData, Roles>,
    #[cfg(feature = "egl")]
    active_egl_context: Rc<RefCell<Option<EGLDisplay>>>,
    session: AutoSession,
    backends: HashMap<
        dev_t,
        (
            S::Id,
            Source<Generic<EventedFd<RenderDevice>>>,
            Rc<RefCell<HashMap<crtc::Handle, GliumDrawer<RenderSurface>>>>,
        ),
    >,
    display: Rc<RefCell<Display>>,
    primary_gpu: Option<PathBuf>,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    pointer_image: ImageBuffer<Rgba<u8>, Vec<u8>>,
    loop_handle: LoopHandle<Data>,
    notifier: S,
    logger: ::slog::Logger,
}

impl<S: SessionNotifier, Data: 'static> UdevHandlerImpl<S, Data> {
    #[cfg(feature = "egl")]
    pub fn scan_connectors(
        device: &mut RenderDevice,
        egl_display: Rc<RefCell<Option<EGLDisplay>>>,
        logger: &::slog::Logger,
    ) -> HashMap<crtc::Handle, GliumDrawer<RenderSurface>> {
        // Get a set of all modesetting resource handles (excluding planes):
        let res_handles = device.resource_handles().unwrap();

        // Use first connected connector
        let connector_infos: Vec<ConnectorInfo> = res_handles
            .connectors()
            .iter()
            .map(|conn| device.resource_info::<ConnectorInfo>(*conn).unwrap())
            .filter(|conn| conn.connection_state() == ConnectorState::Connected)
            .inspect(|conn| info!(logger, "Connected: {:?}", conn.connector_type()))
            .collect();

        let mut backends = HashMap::new();

        // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
        for connector_info in connector_infos {
            let encoder_infos = connector_info
                .encoders()
                .iter()
                .flat_map(|encoder_handle| device.resource_info::<EncoderInfo>(*encoder_handle))
                .collect::<Vec<EncoderInfo>>();
            for encoder_info in encoder_infos {
                for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                    if !backends.contains_key(&crtc) {
                        let mode = connector_info.modes()[0]; // Use first mode (usually highest resoltion, but in reality you should filter and sort and check and match with other connectors, if you use more then one.)
                                                              // create a backend
                        let renderer = GliumDrawer::init(
                            device
                                .create_surface(crtc, mode, vec![connector_info.handle()].into_iter())
                                .unwrap(),
                            egl_display.clone(),
                            logger.clone(),
                        );

                        backends.insert(crtc, renderer);
                        break;
                    }
                }
            }
        }

        backends
    }

    #[cfg(not(feature = "egl"))]
    pub fn scan_connectors(
        device: &mut RenderDevice,
        logger: &::slog::Logger,
    ) -> HashMap<crtc::Handle, GliumDrawer<RenderSurface>> {
        // Get a set of all modesetting resource handles (excluding planes):
        let res_handles = device.resource_handles().unwrap();

        // Use first connected connector
        let connector_infos: Vec<ConnectorInfo> = res_handles
            .connectors()
            .iter()
            .map(|conn| device.resource_info::<ConnectorInfo>(*conn).unwrap())
            .filter(|conn| conn.connection_state() == ConnectorState::Connected)
            .inspect(|conn| info!(logger, "Connected: {:?}", conn.connector_type()))
            .collect();

        let mut backends = HashMap::new();

        // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
        for connector_info in connector_infos {
            let encoder_infos = connector_info
                .encoders()
                .iter()
                .flat_map(|encoder_handle| device.resource_info::<EncoderInfo>(*encoder_handle))
                .collect::<Vec<EncoderInfo>>();
            for encoder_info in encoder_infos {
                for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                    if !backends.contains_key(&crtc) {
                        let mode = connector_info.modes()[0]; // Use first mode (usually highest resoltion, but in reality you should filter and sort and check and match with other connectors, if you use more then one.)
                                                              // create a backend
                        let renderer = GliumDrawer::init(
                            device
                                .create_surface(crtc, mode, vec![connector_info.handle()].into_iter())
                                .unwrap(),
                            logger.clone(),
                        );

                        backends.insert(crtc, renderer);
                        break;
                    }
                }
            }
        }

        backends
    }
}

impl<S: SessionNotifier, Data: 'static> UdevHandler for UdevHandlerImpl<S, Data> {
    fn device_added(&mut self, _device: dev_t, path: PathBuf) {
        if let Some(mut device) = self
            .session
            .open(
                &path,
                OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            ).ok()
            .and_then(|fd| LegacyDrmDevice::new(SessionFd(fd), self.logger.clone()).ok())
            .and_then(|drm| GbmDevice::new(drm, self.logger.clone()).ok())
            .and_then(|gbm| EglDevice::new(gbm, self.logger.clone()).ok())
        {
            // init hardware acceleration on the primary gpu.
            #[cfg(feature = "egl")]
            {
                if path.canonicalize().ok() == self.primary_gpu {
                    *self.active_egl_context.borrow_mut() =
                        device.bind_wl_display(&*self.display.borrow()).ok();
                }
            }

            #[cfg(feature = "egl")]
            let backends = Rc::new(RefCell::new(UdevHandlerImpl::<S, Data>::scan_connectors(
                &mut device,
                self.active_egl_context.clone(),
                &self.logger,
            )));

            #[cfg(not(feature = "egl"))]
            let backends = Rc::new(RefCell::new(UdevHandlerImpl::<S, Data>::scan_connectors(
                &mut device,
                &self.logger,
            )));

            device.set_handler(DrmHandlerImpl {
                compositor_token: self.compositor_token,
                backends: backends.clone(),
                window_map: self.window_map.clone(),
                pointer_location: self.pointer_location.clone(),
                logger: self.logger.clone(),
            });

            let device_session_id = self.notifier.register(device.observer());
            let dev_id = device.device_id();
            let event_source = device_bind(&self.loop_handle, device)
                .map_err(|e| -> IoError { e.into() })
                .unwrap();

            for renderer in backends.borrow_mut().values() {
                // create cursor
                renderer
                    .borrow()
                    .set_cursor_representation(&self.pointer_image, (2, 2))
                    .unwrap();

                // render first frame
                {
                    let mut frame = renderer.draw();
                    frame.clear_color(0.8, 0.8, 0.9, 1.0);
                    frame.finish().unwrap();
                }
            }

            self.backends
                .insert(dev_id, (device_session_id, event_source, backends));
        }
    }

    fn device_changed(&mut self, device: dev_t) {
        //quick and dirty, just re-init all backends
        if let Some((_, ref mut evt_source, ref backends)) = self.backends.get_mut(&device) {
            let source = evt_source.clone_inner();
            let mut evented = source.borrow_mut();
            let mut backends = backends.borrow_mut();
            #[cfg(feature = "egl")]
            let new_backends = UdevHandlerImpl::<S, Data>::scan_connectors(
                &mut (*evented).0,
                self.active_egl_context.clone(),
                &self.logger,
            );
            #[cfg(not(feature = "egl"))]
            let new_backends = UdevHandlerImpl::<S, Data>::scan_connectors(&mut (*evented).0, &self.logger);
            *backends = new_backends;

            for renderer in backends.values() {
                // create cursor
                renderer
                    .borrow()
                    .set_cursor_representation(&self.pointer_image, (2, 2))
                    .unwrap();

                // render first frame
                {
                    let mut frame = renderer.draw();
                    frame.clear_color(0.8, 0.8, 0.9, 1.0);
                    frame.finish().unwrap();
                }
            }
        }
    }

    fn device_removed(&mut self, device: dev_t) {
        // drop the backends on this side
        if let Some((id, evt_source, renderers)) = self.backends.remove(&device) {
            // drop surfaces
            renderers.borrow_mut().clear();
            debug!(self.logger, "Surfaces dropped");

            // don't use hardware acceleration anymore, if this was the primary gpu
            let device = Rc::try_unwrap(evt_source.remove().unwrap())
                .map_err(|_| "This should not happend")
                .unwrap()
                .into_inner()
                .0;

            #[cfg(feature = "egl")]
            {
                if device.dev_path().and_then(|path| path.canonicalize().ok()) == self.primary_gpu {
                    *self.active_egl_context.borrow_mut() = None;
                }
            }

            self.notifier.unregister(id);
            debug!(self.logger, "Dropping device");
        }
    }
}

pub struct DrmHandlerImpl {
    compositor_token: CompositorToken<SurfaceData, Roles>,
    backends: Rc<RefCell<HashMap<crtc::Handle, GliumDrawer<RenderSurface>>>>,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    logger: ::slog::Logger,
}

impl DeviceHandler for DrmHandlerImpl {
    type Device = RenderDevice;

    fn vblank(&mut self, crtc: crtc::Handle) {
        if let Some(drawer) = self.backends.borrow().get(&crtc) {
            {
                let (x, y) = *self.pointer_location.borrow();
                let _ = drawer
                    .borrow()
                    .set_cursor_position(x.trunc().abs() as u32, y.trunc().abs() as u32);
            }

            drawer.draw_windows(&*self.window_map.borrow(), self.compositor_token, &self.logger);
        }
    }

    fn error(&mut self, error: <RenderSurface as Surface>::Error) {
        error!(self.logger, "{:?}", error);
    }
}
