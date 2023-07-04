use std::{process::Command, sync::atomic::Ordering, time::Duration};

use event_channel::EventChannelReceiver;
use internal_action::InternalAction;
use leftwm_config::LeftwmConfig;
use leftwm_core::{
    models::{FocusBehaviour, WindowHandle},
    DisplayAction, DisplayEvent, DisplayServer, Window,
};
use smithay::{
    backend::{
        input::{Event, InputEvent, KeyState, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::UdevBackend,
    },
    input::keyboard::{xkb, FilterResult},
    reexports::{
        calloop::{
            channel::{self, Sender as CalloopSender},
            EventLoop,
        },
        input::{Led, Libinput},
        wayland_server::Display,
    },
    utils::SERIAL_COUNTER,
};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::state::{CalloopData, SmithayState};
mod drawing;
mod event_channel;
mod handlers;
mod input_handler;
mod internal_action;
mod leftwm_config;
mod managed_window;
mod pointer;
mod state;
mod udev;
mod window_registry;

// FIXME: For some reason windows are placed at an offset, I have now idea why. This const corrects
// for that offset.
const OFFSET: i32 = 11;

pub struct SmithayHandle {
    event_receiver: EventChannelReceiver,
    action_sender: CalloopSender<InternalAction>,
}

impl DisplayServer for SmithayHandle {
    fn new(config: &impl leftwm_core::Config) -> Self {
        let (event_sender, event_receiver) = event_channel::event_channel();
        let (init_notify_sender, init_notify_receiver) = oneshot::channel::<()>();
        let (action_sender, action_receiver) = channel::channel::<InternalAction>();

        let config = LeftwmConfig {
            focus_behavior: config.focus_behaviour(),
            sloppy_mouse_follows_focus: config.sloppy_mouse_follows_focus(),
        };

        std::thread::spawn(move || {
            let mut event_loop = EventLoop::<CalloopData>::try_new().unwrap();
            let mut display = Display::<SmithayState>::new().unwrap();

            let (session, notifier) = LibSeatSession::new().unwrap();

            let mut state = SmithayState::init(
                event_sender,
                &mut display,
                udev::init_udev_stage_1(session),
                config,
                event_loop.handle(),
                event_loop.get_signal(),
            );

            let udev_backend = match UdevBackend::new(&state.seat_name) {
                Ok(ret) => ret,
                Err(err) => {
                    panic!("Failed to initialize udev backend: {}", err);
                }
            };

            let mut libinput_context = Libinput::new_with_udev::<
                LibinputSessionInterface<LibSeatSession>,
            >(state.udev_data.session.clone().into());
            libinput_context
                .udev_assign_seat(&state.udev_data.session.seat())
                .unwrap();

            let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

            event_loop
                .handle()
                .insert_source(libinput_backend, move |event, _, calloopdata| {
                    match event {
                        InputEvent::Keyboard { event, .. } => {
                            let serial = SERIAL_COUNTER.next_serial();
                            let time = Event::time_msec(&event);
                            if let Some(vt) = calloopdata.state.seat.get_keyboard().unwrap().input(
                                &mut calloopdata.state,
                                event.key_code(),
                                event.state(),
                                serial,
                                time,
                                |state, modifiers, handle| {
                                    if event.state() == KeyState::Pressed {
                                        let mut leds = Led::empty();
                                        if modifiers.caps_lock {
                                            leds.insert(Led::CAPSLOCK);
                                        }
                                        if modifiers.num_lock {
                                            leds.insert(Led::NUMLOCK);
                                        }
                                        event.device().led_update(leds);
                                        if modifiers.logo
                                            && modifiers.shift
                                            && handle.modified_sym() == xkb::KEY_Return
                                        {
                                            Command::new("kitty").spawn().unwrap();
                                        } else if modifiers.logo
                                            && modifiers.shift
                                            && handle.modified_sym() == xkb::KEY_Q
                                        {
                                            info!("Exiting");
                                            state.running.store(false, Ordering::SeqCst);
                                        } else if (xkb::KEY_XF86Switch_VT_1
                                            ..=xkb::KEY_XF86Switch_VT_12)
                                            .contains(&handle.modified_sym())
                                        {
                                            // VTSwitch
                                            let vt = (handle.modified_sym()
                                                - xkb::KEY_XF86Switch_VT_1
                                                + 1)
                                                as i32;
                                            return FilterResult::Intercept(vt);
                                        }
                                    }
                                    FilterResult::Forward
                                },
                            ) {
                                calloopdata.state.udev_data.session.change_vt(vt).unwrap();
                            };
                        }
                        InputEvent::PointerMotion { event } => {
                            calloopdata
                                .state
                                .on_pointer_move::<LibinputInputBackend>(event);
                        }
                        InputEvent::PointerMotionAbsolute { event } => {
                            todo!()
                        }
                        InputEvent::DeviceAdded { mut device } => {
                            device.config_tap_set_enabled(true).ok();
                            device.config_tap_set_drag_enabled(true).ok();
                        }
                        _ => {}
                    };
                })
                .unwrap();

            let handle = event_loop.handle();
            event_loop
                .handle()
                .insert_source(notifier, move |event, &mut (), data| match event {
                    SessionEvent::PauseSession => {
                        libinput_context.suspend();
                        info!("pausing session");

                        for backend in data.state.udev_data.devices.values() {
                            backend.drm.pause();
                        }
                    }
                    SessionEvent::ActivateSession => {
                        info!("resuming session");

                        if let Err(err) = libinput_context.resume() {
                            error!("Failed to resume libinput context: {:?}", err);
                        }
                        for (node, backend) in data
                            .state
                            .udev_data
                            .devices
                            .iter_mut()
                            .map(|(handle, backend)| (*handle, backend))
                        {
                            backend.drm.activate();
                            for surface in backend.surfaces.values_mut() {
                                if let Err(err) = surface.compositor.surface().reset_state() {
                                    warn!("Failed to reset drm surface state: {}", err);
                                }
                                // reset the buffers after resume to trigger a full redraw
                                // this is important after a vt switch as the primary plane
                                // has no content and damage tracking may prevent a redraw
                                // otherwise
                                surface.compositor.reset_buffers();
                            }
                            handle.insert_idle(move |data| data.state.render(node, None));
                        }
                    }
                })
                .unwrap();

            state.init_udev_stage_2(udev_backend, &display);

            event_loop
                .handle()
                .insert_source(action_receiver, |event, _, data| match event {
                    channel::Event::Msg(act) => {
                        // info!("Received action from leftwm: {:#?}", act);
                        match act {
                            InternalAction::Flush => data.display.flush_clients().unwrap(),
                            InternalAction::GenerateVerifyFocusEvent => (), //TODO: implement
                            InternalAction::UpdateWindows(windows) => {
                                info!("Received window update: {:#?}", windows);
                                for window in windows {
                                    let WindowHandle::SmithayHandle(handle) = window.handle else {
                                        panic!("LeftWM passed an invalid handle");
                                    };
                                    let managed_window =
                                        data.state.window_registry.get(handle).unwrap();
                                    data.state.space.unmap_elem(managed_window);
                                    data.state.space.map_element(
                                        managed_window.clone(),
                                        // FIXME: For some reason windows are placed at an offset,
                                        // I have now idea why
                                        (window.x() - OFFSET, window.y() - OFFSET),
                                        false,
                                    );

                                    managed_window
                                        .window
                                        .toplevel()
                                        .with_pending_state(|state| {
                                            state.size =
                                                Some((window.width(), window.height()).into());
                                        });
                                    managed_window.window.toplevel().send_configure();
                                }
                            }
                            InternalAction::DisplayAction(DisplayAction::KillWindow(_)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::AddedWindow(
                                handle,
                                floating,
                                focus,
                            )) => {
                                let WindowHandle::SmithayHandle(handle) = handle else {
                                    panic!("LeftWM passed an invalid handle");
                                };
                                let window = data.state.window_registry.get_mut(handle).unwrap();
                                let mut window_data = window.data.write().unwrap();
                                window_data.floating = floating;
                                window_data.managed = true;
                                drop(window_data);
                                drop(window);
                                if focus {
                                    data.state.focus_window(handle, true);
                                }
                            }
                            InternalAction::DisplayAction(DisplayAction::MoveMouseOver(_, _)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::MoveMouseOverPoint(_)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::SetState(_, _, _)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::SetWindowOrder(_, _)) => {
                                //TODO: no `todo!()` here because crash
                            }
                            InternalAction::DisplayAction(DisplayAction::MoveToTop(_)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::DestroyedWindow(_)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::WindowTakeFocus {
                                window,
                                previous_window: _,
                            }) => {
                                let WindowHandle::SmithayHandle(handle) = window.handle else {
                                    panic!("LeftWM passed an invalid handle");
                                };
                                data.state.focus_window(
                                    handle,
                                    data.state.config.sloppy_mouse_follows_focus,
                                );
                            }
                            InternalAction::DisplayAction(DisplayAction::Unfocus(_, _)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(
                                DisplayAction::FocusWindowUnderCursor,
                            ) => {
                                data.state.focus_window_under();
                            }
                            InternalAction::DisplayAction(DisplayAction::ReplayClick(_, _)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::ReadyToResizeWindow(
                                _,
                            )) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::ReadyToMoveWindow(_)) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::SetCurrentTags(_)) => {
                                //TODO: no `todo!()` here because crash
                            }
                            InternalAction::DisplayAction(DisplayAction::SetWindowTag(
                                handle,
                                tag,
                            )) => {
                                info!("Setting tag {:?} for window {:?}", tag, handle);
                                let WindowHandle::SmithayHandle(handle) = handle else {
                                    panic!("LeftWM passed an invalid handle");
                                };
                                let window = data.state.window_registry.get_mut(handle).unwrap();
                                window.data.write().unwrap().tag = tag;
                            }
                            InternalAction::DisplayAction(DisplayAction::NormalMode) => {
                                todo!()
                            }
                            InternalAction::DisplayAction(DisplayAction::ConfigureXlibWindow(
                                _,
                            )) => {
                                todo!()
                            }
                        }
                    }
                    channel::Event::Closed => {
                        info!("LeftWM closed the channel, assuming we're exiting.");
                        data.state.running.store(false, Ordering::SeqCst);
                    }
                })
                .unwrap();

            init_notify_sender.send(()).unwrap();

            while state.running.load(Ordering::SeqCst) {
                let mut calloop_data = CalloopData { state, display };
                let result =
                    event_loop.dispatch(Some(Duration::from_millis(16)), &mut calloop_data);
                CalloopData { state, display } = calloop_data;

                if result.is_err() {
                    state.running.store(false, Ordering::SeqCst);
                } else {
                    state.space.refresh();
                    // state.popups.cleanup();
                    display.flush_clients().unwrap();
                }
            }
        });

        std::env::set_var("XDG_SESSION_TYPE", "wayland");

        init_notify_receiver.blocking_recv().unwrap();

        Self {
            event_receiver,
            action_sender,
        }
    }

    fn get_next_events(&mut self) -> Vec<DisplayEvent> {
        info!("LeftWM is collecting events");
        self.event_receiver.collect_events()
    }

    //NOTE: Adding the `'_` lifetime here requires the `DisplayServer` trait to be modified to add
    //the lifetime there too.
    fn wait_readable(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
        Box::pin(self.event_receiver.wait_readable())
    }

    fn flush(&self) {
        self.action_sender.send(InternalAction::Flush).unwrap();
    }

    fn generate_verify_focus_event(&self) -> Option<DisplayEvent> {
        self.action_sender
            .send(InternalAction::GenerateVerifyFocusEvent)
            .unwrap();
        None
    }

    fn load_config(
        &mut self,
        _config: &impl leftwm_core::Config,
        _focused: Option<&Option<leftwm_core::models::WindowHandle>>,
        _windows: &[leftwm_core::Window],
    ) {
    }

    fn update_windows(&self, windows: Vec<&Window>) {
        let windows = windows.into_iter().map(|w| w.clone()).collect();
        self.action_sender
            .send(InternalAction::UpdateWindows(windows))
            .unwrap()
    }

    fn update_workspaces(&self, _focused: Option<&leftwm_core::Workspace>) {}

    fn execute_action(&mut self, act: DisplayAction) -> Option<DisplayEvent> {
        self.action_sender
            .send(InternalAction::DisplayAction(act))
            .unwrap();
        None
    }
}
