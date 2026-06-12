/*
 *  Copyright 2021 QuantumBadger
 *
 *  Licensed under the Apache License, Version 2.0 (the "License");
 *  you may not use this file except in compliance with the License.
 *  You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 *  Unless required by applicable law or agreed to in writing, software
 *  distributed under the License is distributed on an "AS IS" BASIS,
 *  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  See the License for the specific language governing permissions and
 *  limitations under the License.
 */

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::ffi::CString;
use std::num::NonZeroU32;
use std::rc::Rc;

use glutin::config::{Config, ConfigTemplateBuilder};
use glutin::context::{
    ContextApi,
    ContextAttributesBuilder,
    NotCurrentGlContext,
    PossiblyCurrentContext,
    PossiblyCurrentGlContext,
    Version
};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{
    GlSurface,
    Surface,
    SurfaceAttributesBuilder,
    SwapInterval,
    WindowSurface
};
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::error::EventLoopError;
use winit::event::{
    ElementState as GlutinElementState,
    KeyEvent,
    MouseScrollDelta as GlutinMouseScrollDelta,
    TouchPhase,
    WindowEvent as GlutinWindowEvent
};
use winit::event_loop::{
    ActiveEventLoop,
    ControlFlow,
    EventLoop,
    EventLoopClosed,
    EventLoopProxy
};
use winit::keyboard::{Key, KeyLocation, NamedKey};
use winit::monitor::MonitorHandle;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{
    CursorGrabMode,
    CursorIcon,
    Icon,
    Window as GlutinWindow,
    Window,
    WindowAttributes,
    WindowId,
    WindowLevel
};

use crate::dimen::{IVec2, UVec2, Vec2, Vector2};
use crate::error::{BacktraceError, ErrorMessage};
use crate::glbackend::constants::GL_VERSION;
use crate::glbackend::{GLBackend, GLBackendGlow};
use crate::glwrapper::GLVersion;
use crate::glutin_winit::{DisplayBuilder, GlWindow};
use crate::window::{
    DrawingWindowHandler,
    EventLoopSendError,
    ModifiersState,
    MouseButton,
    MouseCursorType,
    MouseScrollDistance,
    UserEventSender,
    VirtualKeyCode,
    WindowCreationError,
    WindowCreationMode,
    WindowCreationOptions,
    WindowEventLoopAction,
    WindowFullscreenMode,
    WindowHandler,
    WindowHelper,
    WindowPosition,
    WindowSize,
    WindowStartupInfo
};
use crate::GLRenderer;

pub(crate) struct WindowHelperGlutin<UserEventType: 'static>
{
    window: Rc<Window>,
    event_proxy: EventLoopProxy<UserEventGlutin<UserEventType>>,
    redraw_requested: Cell<bool>,
    terminate_requested: bool,
    physical_size: UVec2,
    is_mouse_grabbed: Cell<bool>,
    /// Queue shared with `GlutinApp`, drained after each callback. Window
    /// creation/closing must be deferred: it needs the `ActiveEventLoop`.
    commands: Rc<RefCell<Vec<AppCommand<UserEventType>>>>
}

impl<UserEventType> WindowHelperGlutin<UserEventType>
{
    #[inline]
    pub fn new(
        window: &Rc<Window>,
        event_proxy: EventLoopProxy<UserEventGlutin<UserEventType>>,
        initial_physical_size: UVec2,
        commands: Rc<RefCell<Vec<AppCommand<UserEventType>>>>
    ) -> Self
    {
        WindowHelperGlutin {
            window: Rc::clone(window),
            event_proxy,
            redraw_requested: Cell::new(false),
            terminate_requested: false,
            physical_size: initial_physical_size,
            is_mouse_grabbed: Cell::new(false),
            commands
        }
    }

    pub fn create_window(
        &self,
        title: &str,
        options: WindowCreationOptions,
        handler: Box<dyn WindowHandler<UserEventType>>,
        modal: bool
    )
    {
        #[cfg(target_os = "windows")]
        let owner_hwnd = {
            use raw_window_handle::RawWindowHandle;
            match self.window.window_handle().map(|handle| handle.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get()),
                _ => None
            }
        };

        // Capture the parent's outer rect now, so a CenterOnParent child can
        // be positioned over it once created.
        let parent_rect = self.window.outer_position().ok().map(|pos| {
            let size = self.window.outer_size();
            (
                IVec2::new(pos.x, pos.y),
                UVec2::new(size.width, size.height)
            )
        });

        self.commands
            .borrow_mut()
            .push(AppCommand::CreateWindow(PendingWindow {
                title: title.to_string(),
                options,
                handler,
                modal,
                parent_rect,
                #[cfg(target_os = "windows")]
                owner_hwnd
            }));
    }

    pub fn close_window(&self)
    {
        self.commands
            .borrow_mut()
            .push(AppCommand::CloseWindow(self.window.id()));
    }

    #[inline]
    #[must_use]
    pub fn is_redraw_requested(&self) -> bool
    {
        self.redraw_requested.get()
    }

    #[inline]
    pub fn set_redraw_requested(&mut self, redraw_requested: bool)
    {
        self.redraw_requested.set(redraw_requested);
    }

    #[inline]
    pub fn get_event_loop_action(&self) -> WindowEventLoopAction
    {
        match self.terminate_requested {
            true => WindowEventLoopAction::Exit,
            false => WindowEventLoopAction::Continue
        }
    }

    pub fn terminate_loop(&mut self)
    {
        self.terminate_requested = true;
    }

    pub fn set_icon_from_rgba_pixels(
        &self,
        data: Vec<u8>,
        size: UVec2
    ) -> Result<(), BacktraceError<ErrorMessage>>
    {
        self.window.set_window_icon(Some(
            Icon::from_rgba(data, size.x, size.y).map_err(|err| {
                ErrorMessage::msg_with_cause("Icon data was invalid", err)
            })?
        ));

        Ok(())
    }

    pub fn set_cursor_visible(&self, visible: bool)
    {
        self.window.set_cursor_visible(visible);
    }

    pub fn set_cursor(&self, cursor: MouseCursorType)
    {
        self.window.set_cursor(CursorIcon::from(cursor));
    }

    pub fn set_cursor_grab(
        &self,
        grabbed: bool
    ) -> Result<(), BacktraceError<ErrorMessage>>
    {
        let central_position = self.physical_size / 2;
        self.window
            .set_cursor_position(PhysicalPosition::new(
                central_position.x as i32,
                central_position.y as i32
            ))
            .map_err(|err| {
                ErrorMessage::msg_with_cause(
                    "Failed to move cursor to center of window",
                    err
                )
            })?;

        let result = if grabbed {
            self.window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| self.window.set_cursor_grab(CursorGrabMode::Confined))
        } else {
            self.window.set_cursor_grab(CursorGrabMode::None)
        };

        match result {
            Ok(_) => {
                self.is_mouse_grabbed.set(grabbed);
                if self
                    .event_proxy
                    .send_event(UserEventGlutin::MouseGrabStatusChanged(
                        self.window.id(),
                        grabbed
                    ))
                    .is_err()
                {
                    log::error!("Failed to notify app of cursor grab: event loop closed");
                }
                Ok(())
            }
            Err(err) => Err(ErrorMessage::msg_with_cause("Could not grab cursor", err))
        }
    }

    pub fn set_resizable(&self, resizable: bool)
    {
        self.window.set_resizable(resizable);
    }

    #[inline]
    pub fn request_redraw(&self)
    {
        self.redraw_requested.set(true);
    }

    pub fn set_title(&self, title: &str)
    {
        self.window.set_title(title);
    }

    pub fn set_fullscreen_mode(&self, mode: WindowFullscreenMode)
    {
        let window = &self.window;

        window.set_fullscreen(match mode {
            WindowFullscreenMode::Windowed => None,
            WindowFullscreenMode::FullscreenBorderless => {
                Some(winit::window::Fullscreen::Borderless(None))
            }
        });

        let is_fullscreen = match mode {
            WindowFullscreenMode::Windowed => false,
            WindowFullscreenMode::FullscreenBorderless => true
        };

        if self
            .event_proxy
            .send_event(UserEventGlutin::FullscreenStatusChanged(
                self.window.id(),
                is_fullscreen
            ))
            .is_err()
        {
            log::error!(
                "Failed to notify app of fullscreen status change: event loop closed"
            );
        }
    }

    pub fn set_size_pixels<S: Into<UVec2>>(&self, size: S)
    {
        let size = size.into();

        let _ = self
            .window
            .request_inner_size(PhysicalSize::new(size.x, size.y));
    }

    pub fn get_size_pixels(&self) -> UVec2
    {
        let size = self.window.inner_size();

        UVec2::new(size.width, size.height)
    }

    pub fn set_size_scaled_pixels<S: Into<Vec2>>(&self, size: S)
    {
        let size = size.into();

        let _ = self
            .window
            .request_inner_size(LogicalSize::new(size.x, size.y));
    }

    pub fn set_position_pixels<P: Into<IVec2>>(&self, position: P)
    {
        let position = position.into();

        self.window
            .set_outer_position(PhysicalPosition::new(position.x, position.y));
    }

    pub fn set_position_scaled_pixels<P: Into<Vec2>>(&self, position: P)
    {
        let position = position.into();

        self.window
            .set_outer_position(winit::dpi::LogicalPosition::new(position.x, position.y));
    }

    #[inline]
    #[must_use]
    pub fn get_scale_factor(&self) -> f64
    {
        self.window.scale_factor()
    }

    pub fn create_user_event_sender(&self) -> UserEventSender<UserEventType>
    {
        // Events from this sender are routed back to this helper's window.
        UserEventSender::new(UserEventSenderGlutin::new(
            self.event_proxy.clone(),
            Some(self.window.id())
        ))
    }
}

pub(crate) struct WindowGlutin<UserEventType: 'static>
{
    event_loop: EventLoop<UserEventGlutin<UserEventType>>,
    title: String,
    options: WindowCreationOptions
}

impl<UserEventType: 'static> WindowGlutin<UserEventType>
{
    pub fn new(
        title: &str,
        options: WindowCreationOptions
    ) -> Result<WindowGlutin<UserEventType>, BacktraceError<WindowCreationError>>
    {
        let event_loop: EventLoop<UserEventGlutin<UserEventType>> =
            EventLoop::with_user_event().build()?;

        // The window, GL context and renderer are created once the event loop
        // is running (in `GlutinApp::resumed`), as required by winit's
        // ApplicationHandler model.
        Ok(WindowGlutin {
            event_loop,
            title: title.to_string(),
            options
        })
    }

    pub fn create_user_event_sender(&self) -> UserEventSender<UserEventType>
    {
        // No window exists before the loop runs; route to the main window.
        UserEventSender::new(UserEventSenderGlutin::new(
            self.event_loop.create_proxy(),
            None
        ))
    }

    pub fn run_loop<Handler>(self, handler: Handler) -> !
    where
        Handler: WindowHandler<UserEventType> + 'static
    {
        let event_proxy = self.event_loop.create_proxy();

        let mut app = GlutinApp {
            initial: Some(PendingWindow {
                title: self.title,
                options: self.options,
                handler: Box::new(handler),
                modal: false,
                parent_rect: None,
                #[cfg(target_os = "windows")]
                owner_hwnd: None
            }),
            event_proxy,
            windows: HashMap::new(),
            main_window: None,
            modal_stack: Vec::new(),
            commands: Rc::new(RefCell::new(Vec::new())),
            pending_user_events: Vec::new()
        };

        let result = self.event_loop.run_app(&mut app);

        // Drop the user's handler (and the GL state) before terminating the
        // process.
        drop(app);

        match result {
            Ok(()) => std::process::exit(0),
            Err(err) => {
                log::error!("Exited loop with error: {err:?}");
                std::process::exit(1);
            }
        }
    }
}

/// Everything belonging to one open window, including its GL context and
/// renderer (inside [DrawingWindowHandler]) — both are per-window resources.
struct WindowEntry<UserEventType: 'static>
{
    window: Rc<Window>,
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
    helper: WindowHelper<UserEventType>,
    handler: DrawingWindowHandler<UserEventType, Box<dyn WindowHandler<UserEventType>>>
}

impl<UserEventType> WindowEntry<UserEventType>
{
    /// Dropping the renderer frees GL resources, which must happen with this
    /// window's context current — not whichever context drew last.
    fn make_current_and_drop(self)
    {
        if self.context.make_current(&self.surface).is_err() {
            log::error!("Failed to make GL context current for cleanup");
        }
        drop(self);
    }
}

/// A window waiting to be created (creation needs the [ActiveEventLoop]).
pub(crate) struct PendingWindow<UserEventType: 'static>
{
    title: String,
    options: WindowCreationOptions,
    handler: Box<dyn WindowHandler<UserEventType>>,
    modal: bool,
    /// Parent's outer (position, size) in physical pixels, captured when the
    /// child was queued; used to honor [WindowPosition::CenterOnParent].
    parent_rect: Option<(IVec2, UVec2)>,
    #[cfg(target_os = "windows")]
    owner_hwnd: Option<isize>
}

/// Requests queued by [WindowHelperGlutin] during callbacks, applied by
/// [GlutinApp] once the callback returns.
pub(crate) enum AppCommand<UserEventType: 'static>
{
    CreateWindow(PendingWindow<UserEventType>),
    CloseWindow(WindowId)
}

struct GlutinApp<UserEventType: 'static>
{
    /// The main window, consumed by `resumed`.
    initial: Option<PendingWindow<UserEventType>>,
    event_proxy: EventLoopProxy<UserEventGlutin<UserEventType>>,
    windows: HashMap<WindowId, WindowEntry<UserEventType>>,
    /// Closing the main (first) window exits the app.
    main_window: Option<WindowId>,
    /// App-modal stack: while non-empty, only the top window receives input.
    modal_stack: Vec<WindowId>,
    commands: Rc<RefCell<Vec<AppCommand<UserEventType>>>>,
    /// User events sent from another thread between loop start and `resumed`.
    pending_user_events: Vec<UserEventGlutin<UserEventType>>
}

impl<UserEventType: 'static> GlutinApp<UserEventType>
{
    fn create_window_entry(
        &mut self,
        event_loop: &ActiveEventLoop,
        pending: PendingWindow<UserEventType>
    )
    {
        let is_main = self.main_window.is_none();

        // A creation failure is fatal for the main window, but for a child
        // window the app is already running — log and carry on without it.
        macro_rules! fail {
            ($($arg:tt)*) => {{
                log::error!($($arg)*);
                if is_main {
                    std::process::exit(1);
                }
                return;
            }};
        }

        #[allow(unused_mut)]
        let mut window_attributes =
            build_window_attributes(&pending.title, &pending.options);

        #[cfg(target_os = "windows")]
        if let Some(hwnd) = pending.owner_hwnd {
            use winit::platform::windows::WindowAttributesExtWindows;
            window_attributes = window_attributes.with_owner_window(hwnd);
        }

        let Some((context, window, surface)) =
            create_best_context(&window_attributes, event_loop, &pending.options)
        else {
            fail!("Failed to create a suitable GL context");
        };

        // The window is still hidden at this point, so the monitor-dependent
        // settings applied below aren't visible to the user.
        let Some(primary_monitor) = window.primary_monitor().or_else(|| {
            log::error!("Couldn't find primary monitor. Using first available monitor.");
            window.available_monitors().next()
        }) else {
            fail!("Failed to find any monitor");
        };

        for (num, monitor) in window.available_monitors().enumerate() {
            log::debug!(
                "Monitor #{}{}: {}",
                num,
                if monitor == primary_monitor {
                    " (primary)"
                } else {
                    ""
                },
                match &monitor.name() {
                    None => "<unnamed>",
                    Some(name) => name.as_str()
                }
            );
        }

        match &pending.options.mode {
            WindowCreationMode::Windowed { size, .. } => match size {
                WindowSize::MarginPhysicalPixels(_)
                | WindowSize::MarginScaledPixels(_) => {
                    let _ = window
                        .request_inner_size(compute_window_size(&primary_monitor, size));
                }
                WindowSize::PhysicalPixels(_) | WindowSize::ScaledPixels(_) => {
                    // Already applied via the window attributes.
                }
            },

            WindowCreationMode::FullscreenBorderless => {
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                    Some(primary_monitor.clone())
                )));
            }
        }

        // Keep the GL surface in sync with the size applied above.
        window.resize_surface(&surface, &context);

        if let WindowCreationMode::Windowed {
            position: Some(position),
            ..
        } = &pending.options.mode
        {
            position_window(&primary_monitor, &window, position, pending.parent_rect);
        }

        // Show window after positioning to avoid the window jumping around
        window.set_visible(true);

        // Set the position again to work around an issue on Linux
        if let WindowCreationMode::Windowed {
            position: Some(position),
            ..
        } = &pending.options.mode
        {
            position_window(&primary_monitor, &window, position, pending.parent_rect);
        }

        let glow_context = unsafe {
            glow::Context::from_loader_function(|ptr| {
                context.display().get_proc_address(
                    CString::new(ptr)
                        .expect("Invalid GL function name string")
                        .as_c_str()
                ) as *const _
            })
        };

        let gl_backend: Rc<dyn GLBackend> = Rc::new(GLBackendGlow::new(glow_context));

        if let Some(error_name) = gl_backend.gl_get_error_name() {
            log::warn!(
                "Ignoring error in GL bindings during startup: {}",
                error_name
            );
        }

        let version = unsafe { gl_backend.gl_get_string(GL_VERSION) };

        log::info!("Using OpenGL version: {}", version);

        unsafe {
            gl_backend.gl_enable_debug_message_callback();
        };

        let initial_viewport_size_pixels: UVec2 = window.inner_size().into();

        let renderer = match GLRenderer::new_with_gl_backend(
            initial_viewport_size_pixels,
            gl_backend,
            GLVersion::OpenGL2_0
        ) {
            Ok(renderer) => renderer,
            Err(err) => {
                fail!("Failed to create renderer: {err:?}");
            }
        };

        let window = Rc::new(window);

        let mut helper = WindowHelper::new(WindowHelperGlutin::new(
            &window,
            self.event_proxy.clone(),
            initial_viewport_size_pixels,
            Rc::clone(&self.commands)
        ));

        let mut handler = DrawingWindowHandler::new(pending.handler, renderer);

        handler.on_start(
            &mut helper,
            WindowStartupInfo::new(initial_viewport_size_pixels, window.scale_factor())
        );

        // Ensure the first frame is drawn even if the OS doesn't deliver an
        // initial RedrawRequested for the newly created window.
        helper.inner().set_redraw_requested(true);

        let id = window.id();

        if is_main {
            self.main_window = Some(id);
        }

        if pending.modal {
            self.modal_stack.push(id);
        }

        window.focus_window();

        self.windows.insert(
            id,
            WindowEntry {
                window,
                context,
                surface,
                helper,
                handler
            }
        );
    }

    fn close_window(&mut self, event_loop: &ActiveEventLoop, id: WindowId)
    {
        if Some(id) == self.main_window {
            self.exit_app(event_loop);
            return;
        }

        if let Some(entry) = self.windows.remove(&id) {
            entry.make_current_and_drop();
        }

        self.modal_stack.retain(|window| *window != id);

        // Refocus whatever is now on top of the modal stack.
        if let Some(top) = self.modal_stack.last() {
            if let Some(entry) = self.windows.get(top) {
                entry.window.focus_window();
            }
        }
    }

    fn exit_app(&mut self, event_loop: &ActiveEventLoop)
    {
        // Drop handlers and GL state with each window's context current.
        for (_, entry) in self.windows.drain() {
            entry.make_current_and_drop();
        }
        self.modal_stack.clear();
        event_loop.exit();
    }

    fn process_commands(&mut self, event_loop: &ActiveEventLoop)
    {
        // Loop: an on_start invoked while creating a window may queue more.
        loop {
            let commands: Vec<AppCommand<UserEventType>> =
                self.commands.borrow_mut().drain(..).collect();

            if commands.is_empty() {
                break;
            }

            for command in commands {
                match command {
                    AppCommand::CreateWindow(pending) => {
                        self.create_window_entry(event_loop, pending)
                    }
                    AppCommand::CloseWindow(id) => self.close_window(event_loop, id)
                }
            }
        }
    }

    /// Honors a `terminate_loop()` request made during the last callback,
    /// then applies any window create/close requests it queued.
    fn after_callback(&mut self, event_loop: &ActiveEventLoop, id: WindowId)
    {
        if let Some(entry) = self.windows.get_mut(&id) {
            if let WindowEventLoopAction::Exit =
                entry.helper.inner().get_event_loop_action()
            {
                self.exit_app(event_loop);
                return;
            }
        }

        self.process_commands(event_loop);
    }

    /// Returns the window the event was dispatched to, if any.
    fn dispatch_user_event(
        &mut self,
        event: UserEventGlutin<UserEventType>
    ) -> Option<WindowId>
    {
        match event {
            UserEventGlutin::MouseGrabStatusChanged(id, grabbed) => {
                let entry = self.windows.get_mut(&id)?;
                entry
                    .handler
                    .on_mouse_grab_status_changed(&mut entry.helper, grabbed);
                Some(id)
            }
            UserEventGlutin::FullscreenStatusChanged(id, fullscreen) => {
                let entry = self.windows.get_mut(&id)?;
                entry
                    .handler
                    .on_fullscreen_status_changed(&mut entry.helper, fullscreen);
                Some(id)
            }
            UserEventGlutin::UserEvent(target, event) => {
                let id = target.or(self.main_window)?;
                let entry = self.windows.get_mut(&id)?;
                entry.handler.on_user_event(&mut entry.helper, event);
                Some(id)
            }
        }
    }
}

/// Window events withheld from non-topmost windows while a modal is open.
fn is_blocked_when_modal(event: &GlutinWindowEvent) -> bool
{
    matches!(
        event,
        GlutinWindowEvent::CursorMoved { .. }
            | GlutinWindowEvent::MouseInput { .. }
            | GlutinWindowEvent::MouseWheel { .. }
            | GlutinWindowEvent::KeyboardInput { .. }
            | GlutinWindowEvent::CloseRequested
            | GlutinWindowEvent::Focused(true)
    )
}

impl<UserEventType: 'static> ApplicationHandler<UserEventGlutin<UserEventType>>
    for GlutinApp<UserEventType>
{
    fn resumed(&mut self, event_loop: &ActiveEventLoop)
    {
        // Desktop platforms emit this once after the loop starts.
        let Some(pending) = self.initial.take() else {
            return;
        };

        self.create_window_entry(event_loop, pending);

        for event in std::mem::take(&mut self.pending_user_events) {
            self.dispatch_user_event(event);
        }

        if let Some(id) = self.main_window {
            self.after_callback(event_loop, id);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: GlutinWindowEvent
    )
    {
        // App-modal gating: while a modal window is open, all other windows
        // are deaf to input; trying to interact refocuses the modal instead.
        if let Some(&top) = self.modal_stack.last() {
            if window_id != top && is_blocked_when_modal(&event) {
                let refocus = matches!(
                    event,
                    GlutinWindowEvent::MouseInput {
                        state: GlutinElementState::Pressed,
                        ..
                    } | GlutinWindowEvent::CloseRequested
                        | GlutinWindowEvent::Focused(true)
                );

                if refocus {
                    if let Some(entry) = self.windows.get(&top) {
                        entry.window.focus_window();
                    }
                }

                return;
            }
        }

        if let GlutinWindowEvent::CloseRequested = event {
            // Main window: exits the app. Child window: closes just itself.
            self.close_window(event_loop, window_id);
            return;
        }

        {
            let Some(entry) = self.windows.get_mut(&window_id) else {
                return;
            };

            let WindowEntry {
                window,
                context,
                surface,
                helper,
                handler
            } = entry;

            match event {
                GlutinWindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                    log::info!("Scale factor changed: {:?}", scale_factor);
                    handler.on_scale_factor_changed(helper, scale_factor)
                }

                GlutinWindowEvent::Resized(physical_size) => {
                    log::info!("Resized: {:?}", physical_size);
                    // With several GL contexts, ours may not be the current
                    // one (the resize and immediate draw below touch GL).
                    if context.make_current(surface).is_err() {
                        log::error!("Failed to make GL context current");
                    }
                    if let (Ok(w), Ok(h)) = (
                        NonZeroU32::try_from(physical_size.width),
                        NonZeroU32::try_from(physical_size.height)
                    ) {
                        surface.resize(context, w, h);
                    }
                    helper.inner().physical_size = physical_size.into();
                    handler.on_resize(helper, physical_size.into());

                    // Draw immediately after resize so the window updates
                    // during interactive drag on Windows, where about_to_wait
                    // does not fire until the drag ends.
                    if helper.inner().is_redraw_requested() {
                        helper.inner().set_redraw_requested(false);
                        handler.on_draw(helper);
                        surface.swap_buffers(context).unwrap();
                    }
                }

                GlutinWindowEvent::CursorMoved { position, .. } => {
                    let position = Vector2::new(position.x, position.y).into_f32();

                    if helper.inner().is_mouse_grabbed.get() {
                        let central_position = helper.inner().physical_size / 2;
                        window
                            .set_cursor_position(PhysicalPosition::new(
                                central_position.x as i32,
                                central_position.y as i32
                            ))
                            .unwrap();

                        let position = position - central_position.into_f32();

                        if position.magnitude_squared() > 0.0001 {
                            handler.on_mouse_move(helper, position);
                        }
                    } else {
                        handler.on_mouse_move(helper, position);
                    };
                }

                GlutinWindowEvent::MouseInput { state, button, .. } => match state {
                    GlutinElementState::Pressed => {
                        handler.on_mouse_button_down(helper, button.into())
                    }
                    GlutinElementState::Released => {
                        handler.on_mouse_button_up(helper, button.into())
                    }
                },

                GlutinWindowEvent::MouseWheel {
                    delta,
                    phase: TouchPhase::Moved,
                    ..
                } => {
                    let distance = match delta {
                        GlutinMouseScrollDelta::LineDelta(x, y) => {
                            MouseScrollDistance::Lines {
                                x: x as f64,
                                y: y as f64,
                                z: 0.0
                            }
                        }
                        GlutinMouseScrollDelta::PixelDelta(pos) => {
                            MouseScrollDistance::Pixels {
                                x: pos.x,
                                y: pos.y,
                                z: 0.0
                            }
                        }
                    };

                    handler.on_mouse_wheel_scroll(helper, distance);
                }

                GlutinWindowEvent::KeyboardInput { event, .. } => {
                    let virtual_key_code = VirtualKeyCode::try_from(&event).ok();

                    match event.state {
                        GlutinElementState::Pressed => {
                            if let Some(text) = event.text {
                                text.chars().for_each(|c| {
                                    handler.on_keyboard_char(helper, c);
                                });
                            }

                            if !event.repeat {
                                handler.on_key_down(
                                    helper,
                                    virtual_key_code,
                                    event.physical_key.to_scancode().unwrap_or(0)
                                );
                            }
                        }
                        GlutinElementState::Released => {
                            handler.on_key_up(
                                helper,
                                virtual_key_code,
                                event.physical_key.to_scancode().unwrap_or(0)
                            );
                        }
                    }
                }

                GlutinWindowEvent::ModifiersChanged(state) => {
                    handler.on_keyboard_modifiers_changed(helper, state.state().into())
                }

                GlutinWindowEvent::RedrawRequested => {
                    helper.inner().set_redraw_requested(true);
                }

                _ => {}
            }
        }

        self.after_callback(event_loop, window_id);
    }

    fn user_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: UserEventGlutin<UserEventType>
    )
    {
        if self.main_window.is_none() {
            // The sender exists before the loop starts; buffer anything that
            // arrives before `resumed` has created the main window.
            self.pending_user_events.push(event);
            return;
        }

        if let Some(id) = self.dispatch_user_event(event) {
            self.after_callback(event_loop, id);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop)
    {
        let mut terminate = false;

        for entry in self.windows.values_mut() {
            if entry.helper.inner().is_redraw_requested() {
                entry.helper.inner().set_redraw_requested(false);

                if entry.context.make_current(&entry.surface).is_err() {
                    log::error!("Failed to make GL context current");
                    continue;
                }

                entry.handler.on_draw(&mut entry.helper);
                entry.surface.swap_buffers(&entry.context).unwrap();
            }

            if let WindowEventLoopAction::Exit =
                entry.helper.inner().get_event_loop_action()
            {
                terminate = true;
            }
        }

        if terminate {
            self.exit_app(event_loop);
            return;
        }

        self.process_commands(event_loop);

        // Poll if any window asked for another frame during its draw.
        let any_redraw = self
            .windows
            .values_mut()
            .any(|entry| entry.helper.inner().is_redraw_requested());

        event_loop.set_control_flow(if any_redraw {
            ControlFlow::Poll
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop)
    {
        // Drop handlers and GL state with each window's context current.
        for (_, entry) in self.windows.drain() {
            entry.make_current_and_drop();
        }
    }
}

fn build_window_attributes(
    title: &str,
    options: &WindowCreationOptions
) -> WindowAttributes
{
    let mut window_attributes = Window::default_attributes()
        .with_title(title)
        .with_resizable(options.resizable)
        .with_window_level(
            if options.always_on_top {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            }
        )
        .with_maximized(options.maximized)
        .with_visible(false)
        .with_transparent(options.transparent)
        .with_decorations(options.decorations);

    // Sizes that don't depend on monitor dimensions are applied at creation
    // time. The margin-based sizes and the fullscreen mode need monitor
    // information, which in winit 0.30 is only available once a window
    // exists, so they are applied after creation (see `resumed`).
    if let WindowCreationMode::Windowed { size, .. } = &options.mode {
        match size {
            WindowSize::PhysicalPixels(size) => {
                window_attributes = window_attributes
                    .with_inner_size(PhysicalSize::new(size.x, size.y));
            }
            WindowSize::ScaledPixels(size) => {
                window_attributes = window_attributes
                    .with_inner_size(LogicalSize::new(size.x, size.y));
            }
            WindowSize::MarginPhysicalPixels(_) | WindowSize::MarginScaledPixels(_) => {
            }
        }
    }

    window_attributes
}

fn gl_config_picker(mut configs: Box<dyn Iterator<Item = Config> + '_>)
    -> Option<Config>
{
    configs.next()
}

fn create_best_context(
    window_attributes: &WindowAttributes,
    event_loop: &ActiveEventLoop,
    options: &WindowCreationOptions
) -> Option<(PossiblyCurrentContext, Window, Surface<WindowSurface>)>
{
    for multisampling in &[options.multisampling, 16, 8, 4, 2, 1, 0] {
        log::info!("Trying multisampling={}...", multisampling);

        let mut template = ConfigTemplateBuilder::new();

        if *multisampling > 1 {
            template = template.with_multisampling(
                (*multisampling)
                    .try_into()
                    .expect("Multisampling level out of bounds")
            );
        }

        let result = DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes.clone()))
            .build(event_loop, template, gl_config_picker);

        let (window, gl_config) = match result {
            Ok((Some(window), config)) => {
                log::info!("Window created");
                (window, config)
            }
            Ok((None, _)) => {
                log::info!("Failed with null window");
                continue;
            }
            Err(err) => {
                log::info!("Failed with error: {:?}", err);
                continue;
            }
        };

        let gl_display = gl_config.display();

        let raw_window_handle = match window.window_handle() {
            Ok(handle) => Some(handle.as_raw()),
            Err(err) => {
                log::info!("Failed to get window handle: {err:?}");
                None
            }
        };

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(2, 0))))
            .build(raw_window_handle);

        let context =
            match unsafe { gl_display.create_context(&gl_config, &context_attributes) } {
                Ok(context) => context,
                Err(err) => {
                    log::info!("Failed to create context with error: {err:?}");
                    continue;
                }
            };

        let window = match crate::glutin_winit::finalize_window(
            event_loop,
            window_attributes.clone(),
            &gl_config
        ) {
            Ok(window) => window,
            Err(err) => {
                log::info!("Failed to finalize window with error: {err:?}");
                continue;
            }
        };

        let attrs =
            match window.build_surface_attributes(SurfaceAttributesBuilder::default()) {
                Ok(attrs) => attrs,
                Err(err) => {
                    log::info!("Failed to build surface attributes with error: {err:?}");
                    continue;
                }
            };

        let surface = match unsafe {
            gl_config
                .display()
                .create_window_surface(&gl_config, &attrs)
        } {
            Ok(surface) => surface,
            Err(err) => {
                log::info!("Failed to finalize surface with error: {err:?}");
                continue;
            }
        };

        let context = match context.make_current(&surface) {
            Ok(context) => context,
            Err(err) => {
                log::info!("Failed to make context current with error: {err:?}");
                continue;
            }
        };

        if options.vsync {
            if let Err(err) = surface.set_swap_interval(
                &context,
                SwapInterval::Wait(NonZeroU32::new(1).unwrap())
            ) {
                log::error!("Error setting vsync, continuing anyway: {err:?}");
            }
        }

        return Some((context, window, surface));
    }

    log::error!("Failed to create any context.");
    None
}

fn position_window(
    monitor: &MonitorHandle,
    window: &GlutinWindow,
    position: &WindowPosition,
    parent_rect: Option<(IVec2, UVec2)>
)
{
    let monitor_position = monitor.position();

    let center_on_monitor = |window: &GlutinWindow| {
        let monitor_size = monitor.size();
        let outer_size = window.outer_size();

        log::info!(
            "Centering window. Monitor size: {:?}. Window outer size: {:?}.",
            monitor_size,
            outer_size
        );

        window.set_outer_position(PhysicalPosition::new(
            monitor_position.x
                + ((monitor_size.width as i32 - outer_size.width as i32) / 2),
            monitor_position.y
                + ((monitor_size.height as i32 - outer_size.height as i32) / 2)
        ));
    };

    match position {
        WindowPosition::Center => center_on_monitor(window),

        WindowPosition::CenterOnParent => match parent_rect {
            Some((parent_pos, parent_size)) => {
                let outer_size = window.outer_size();
                window.set_outer_position(PhysicalPosition::new(
                    parent_pos.x
                        + ((parent_size.x as i32 - outer_size.width as i32) / 2),
                    parent_pos.y
                        + ((parent_size.y as i32 - outer_size.height as i32) / 2)
                ));
            }
            // No parent (e.g. the main window): behave like Center.
            None => center_on_monitor(window)
        },

        WindowPosition::PrimaryMonitorPixelsFromTopLeft(position) => window
            .set_outer_position(PhysicalPosition::new(
                monitor_position.x + position.x,
                monitor_position.y + position.y
            ))
    }
}

fn compute_window_size(monitor: &MonitorHandle, size: &WindowSize) -> PhysicalSize<u32>
{
    let monitor_size = monitor.size();

    match size {
        WindowSize::PhysicalPixels(size) => PhysicalSize::new(size.x, size.y),

        WindowSize::ScaledPixels(size) => {
            LogicalSize::new(size.x, size.y).to_physical(monitor.scale_factor())
        }

        WindowSize::MarginPhysicalPixels(margin) => {
            let margin_physical_px = std::cmp::min(
                *margin,
                std::cmp::min(monitor_size.width, monitor_size.height) / 4
            );

            PhysicalSize::new(
                monitor_size.width - 2 * margin_physical_px,
                monitor_size.height - 2 * margin_physical_px
            )
        }

        WindowSize::MarginScaledPixels(margin) => {
            let margin_physical_px = std::cmp::min(
                (*margin as f64 * monitor.scale_factor()).round() as u32,
                std::cmp::min(monitor_size.width, monitor_size.height) / 4
            );

            PhysicalSize::new(
                monitor_size.width - 2 * margin_physical_px,
                monitor_size.height - 2 * margin_physical_px
            )
        }
    }
}

impl From<winit::event::MouseButton> for MouseButton
{
    fn from(button: winit::event::MouseButton) -> Self
    {
        match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Right => MouseButton::Right,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Other(id) => MouseButton::Other(id),
            winit::event::MouseButton::Back => MouseButton::Back,
            winit::event::MouseButton::Forward => MouseButton::Forward
        }
    }
}

impl From<MouseCursorType> for CursorIcon
{
    fn from(cursor: MouseCursorType) -> Self
    {
        match cursor {
            MouseCursorType::Default => CursorIcon::Default,
            MouseCursorType::Pointer => CursorIcon::Pointer,
            MouseCursorType::Crosshair => CursorIcon::Crosshair,
            MouseCursorType::Text => CursorIcon::Text,
            MouseCursorType::VerticalText => CursorIcon::VerticalText,
            MouseCursorType::Move => CursorIcon::Move,
            MouseCursorType::Grab => CursorIcon::Grab,
            MouseCursorType::Grabbing => CursorIcon::Grabbing,
            MouseCursorType::Wait => CursorIcon::Wait,
            MouseCursorType::Progress => CursorIcon::Progress,
            MouseCursorType::Cell => CursorIcon::Cell,
            MouseCursorType::Alias => CursorIcon::Alias,
            MouseCursorType::Copy => CursorIcon::Copy,
            MouseCursorType::NoDrop => CursorIcon::NoDrop,
            MouseCursorType::NotAllowed => CursorIcon::NotAllowed,
            MouseCursorType::ColResize => CursorIcon::ColResize,
            MouseCursorType::RowResize => CursorIcon::RowResize,
            MouseCursorType::EwResize => CursorIcon::EwResize,
            MouseCursorType::NsResize => CursorIcon::NsResize,
            MouseCursorType::NeswResize => CursorIcon::NeswResize,
            MouseCursorType::NwseResize => CursorIcon::NwseResize,
            MouseCursorType::ZoomIn => CursorIcon::ZoomIn,
            MouseCursorType::ZoomOut => CursorIcon::ZoomOut
        }
    }
}

impl TryFrom<&KeyEvent> for VirtualKeyCode
{
    type Error = ();

    fn try_from(event: &KeyEvent) -> Result<Self, Self::Error>
    {
        let lr_variant =
            |left: VirtualKeyCode, right: VirtualKeyCode| match event.location {
                KeyLocation::Standard | KeyLocation::Left => left,
                KeyLocation::Right | KeyLocation::Numpad => right
            };

        let numpad_variant =
            |normal: VirtualKeyCode, numpad: VirtualKeyCode| match event.location {
                KeyLocation::Standard | KeyLocation::Left | KeyLocation::Right => normal,
                KeyLocation::Numpad => numpad
            };

        Ok(match event.logical_key.clone() {
            Key::Named(virtual_key_code) => match virtual_key_code {
                NamedKey::Alt => lr_variant(Self::LAlt, Self::RAlt),
                NamedKey::AltGraph => Self::RAlt,
                NamedKey::ArrowDown => Self::Down,
                NamedKey::ArrowLeft => Self::Left,
                NamedKey::ArrowRight => Self::Right,
                NamedKey::ArrowUp => Self::Up,
                NamedKey::AudioVolumeDown => Self::VolumeDown,
                NamedKey::AudioVolumeMute => Self::Mute,
                NamedKey::AudioVolumeUp => Self::VolumeUp,
                NamedKey::Backspace => Self::Backspace,
                NamedKey::BrowserBack => Self::WebBack,
                NamedKey::BrowserFavorites => Self::WebFavorites,
                NamedKey::BrowserForward => Self::WebForward,
                NamedKey::BrowserHome => Self::WebHome,
                NamedKey::BrowserRefresh => Self::WebRefresh,
                NamedKey::BrowserSearch => Self::WebSearch,
                NamedKey::BrowserStop => Self::WebStop,
                NamedKey::Compose => Self::Compose,
                NamedKey::Control => lr_variant(Self::LControl, Self::RControl),
                NamedKey::Convert => Self::Convert,
                NamedKey::Copy => Self::Copy,
                NamedKey::Cut => Self::Cut,
                NamedKey::Delete => Self::Delete,
                NamedKey::End => Self::End,
                NamedKey::Enter => numpad_variant(Self::Return, Self::NumpadEnter),
                NamedKey::Escape => Self::Escape,
                NamedKey::F1 => Self::F1,
                NamedKey::F2 => Self::F2,
                NamedKey::F3 => Self::F3,
                NamedKey::F4 => Self::F4,
                NamedKey::F5 => Self::F5,
                NamedKey::F6 => Self::F6,
                NamedKey::F7 => Self::F7,
                NamedKey::F8 => Self::F8,
                NamedKey::F9 => Self::F9,
                NamedKey::F10 => Self::F10,
                NamedKey::F11 => Self::F11,
                NamedKey::F12 => Self::F12,
                NamedKey::F13 => Self::F13,
                NamedKey::F14 => Self::F14,
                NamedKey::F15 => Self::F15,
                NamedKey::F16 => Self::F16,
                NamedKey::F17 => Self::F17,
                NamedKey::F18 => Self::F18,
                NamedKey::F19 => Self::F19,
                NamedKey::F20 => Self::F20,
                NamedKey::F21 => Self::F21,
                NamedKey::F22 => Self::F22,
                NamedKey::F23 => Self::F23,
                NamedKey::F24 => Self::F24,
                NamedKey::GoBack => Self::NavigateBackward,
                NamedKey::GoHome => Self::Home,
                NamedKey::Home => Self::Home,
                NamedKey::Insert => Self::Insert,
                NamedKey::KanaMode => Self::Kana,
                NamedKey::KanjiMode => Self::Kanji,
                NamedKey::LaunchMail => Self::Mail,
                NamedKey::MediaPlayPause => Self::PlayPause,
                NamedKey::MediaStop => Self::MediaStop,
                NamedKey::NavigatePrevious => Self::NavigateBackward,
                NamedKey::NonConvert => Self::NoConvert,
                NamedKey::NumLock => Self::Numlock,
                NamedKey::PageDown => Self::PageDown,
                NamedKey::PageUp => Self::PageUp,
                NamedKey::Paste => Self::Paste,
                NamedKey::Power => Self::Power,
                NamedKey::PrintScreen => Self::PrintScreen,
                NamedKey::ScrollLock => Self::ScrollLock,
                NamedKey::Shift => lr_variant(Self::LShift, Self::RShift),
                NamedKey::Tab => Self::Tab,
                NamedKey::Super => lr_variant(Self::LWin, Self::RWin),
                _ => return Err(())
            },
            Key::Character(c) => match c.chars().next().unwrap_or('\0') {
                'A' | 'a' => Self::A,
                'B' | 'b' => Self::B,
                'C' | 'c' => Self::C,
                'D' | 'd' => Self::D,
                'E' | 'e' => Self::E,
                'F' | 'f' => Self::F,
                'G' | 'g' => Self::G,
                'H' | 'h' => Self::H,
                'I' | 'i' => Self::I,
                'J' | 'j' => Self::J,
                'K' | 'k' => Self::K,
                'L' | 'l' => Self::L,
                'M' | 'm' => Self::M,
                'N' | 'n' => Self::N,
                'O' | 'o' => Self::O,
                'P' | 'p' => Self::P,
                'Q' | 'q' => Self::Q,
                'R' | 'r' => Self::R,
                'S' | 's' => Self::S,
                'T' | 't' => Self::T,
                'U' | 'u' => Self::U,
                'V' | 'v' => Self::V,
                'W' | 'w' => Self::W,
                'X' | 'x' => Self::X,
                'Y' | 'y' => Self::Y,
                'Z' | 'z' => Self::Z,
                '0' => numpad_variant(Self::Key0, Self::Numpad0),
                '1' => numpad_variant(Self::Key1, Self::Numpad1),
                '2' => numpad_variant(Self::Key2, Self::Numpad2),
                '3' => numpad_variant(Self::Key3, Self::Numpad3),
                '4' => numpad_variant(Self::Key4, Self::Numpad4),
                '5' => numpad_variant(Self::Key5, Self::Numpad5),
                '6' => numpad_variant(Self::Key6, Self::Numpad6),
                '7' => numpad_variant(Self::Key7, Self::Numpad7),
                '8' => numpad_variant(Self::Key8, Self::Numpad8),
                '9' => numpad_variant(Self::Key9, Self::Numpad9),
                '+' => numpad_variant(Self::Plus, Self::NumpadAdd),
                '-' => numpad_variant(Self::Minus, Self::NumpadSubtract),
                '*' => numpad_variant(Self::Asterisk, Self::NumpadMultiply),
                '/' => numpad_variant(Self::Slash, Self::NumpadDivide),
                ',' => numpad_variant(Self::Comma, Self::NumpadComma),
                '.' => numpad_variant(Self::Period, Self::NumpadDecimal),
                '=' => numpad_variant(Self::Equals, Self::NumpadEquals),
                '^' => Self::Caret,
                '\'' => Self::Apostrophe,
                '\\' => Self::Backslash,
                ':' => Self::Colon,
                '`' => Self::Grave,
                '(' => Self::LBracket,
                ')' => Self::RBracket,
                '\t' => Self::Tab,

                _ => return Err(())
            },
            Key::Unidentified(_) | Key::Dead(_) => return Err(())
        })
    }
}

impl From<winit::keyboard::ModifiersState> for ModifiersState
{
    fn from(state: winit::keyboard::ModifiersState) -> Self
    {
        ModifiersState {
            ctrl: state.control_key(),
            alt: state.alt_key(),
            shift: state.shift_key(),
            logo: state.super_key()
        }
    }
}

impl From<PhysicalSize<u32>> for UVec2
{
    fn from(value: PhysicalSize<u32>) -> Self
    {
        Self::new(value.width, value.height)
    }
}

pub(crate) enum UserEventGlutin<UserEventType: 'static>
{
    MouseGrabStatusChanged(WindowId, bool),
    FullscreenStatusChanged(WindowId, bool),
    /// `None` targets the main window (e.g. senders created before the loop
    /// started, when no window existed yet).
    UserEvent(Option<WindowId>, UserEventType)
}

pub struct UserEventSenderGlutin<UserEventType: 'static>
{
    event_proxy: EventLoopProxy<UserEventGlutin<UserEventType>>,
    /// The window whose handler receives the events; `None` = main window.
    target: Option<WindowId>
}

impl<UserEventType> Clone for UserEventSenderGlutin<UserEventType>
{
    fn clone(&self) -> Self
    {
        UserEventSenderGlutin {
            event_proxy: self.event_proxy.clone(),
            target: self.target
        }
    }
}

impl<UserEventType> UserEventSenderGlutin<UserEventType>
{
    fn new(
        event_proxy: EventLoopProxy<UserEventGlutin<UserEventType>>,
        target: Option<WindowId>
    ) -> Self
    {
        Self {
            event_proxy,
            target
        }
    }

    pub fn send_event(&self, event: UserEventType) -> Result<(), EventLoopSendError>
    {
        self.event_proxy
            .send_event(UserEventGlutin::UserEvent(self.target, event))
            .map_err(|err| match err {
                EventLoopClosed(_) => EventLoopSendError::EventLoopNoLongerExists
            })
    }
}

impl From<EventLoopError> for BacktraceError<WindowCreationError>
{
    fn from(value: EventLoopError) -> Self
    {
        Self::new_with_cause(WindowCreationError::EventLoopCreationFailed, value)
    }
}
