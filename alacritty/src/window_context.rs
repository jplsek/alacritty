//! Terminal window context.

use std::collections::HashSet;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(unix)]
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use log::info;
use serde_json as json;
use slint::LogicalPosition;
use winit::dpi::PhysicalSize;
use winit::event::{
    ElementState, Event as WinitEvent, Modifiers, MouseButton, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::{CursorIcon, WindowId};

use alacritty_terminal::event::Event as TerminalEvent;
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty;

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::display::Display;
use crate::display::window::Window;
use crate::event::{
    ActionContext, Event, EventProxy, EventType, InlineSearchState, Mouse, SearchState, TabAction,
    TabDrag, TabDrop, TabId, TouchPurpose,
};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::scheduler::Scheduler;
#[cfg(unix)]
use crate::session::WindowSession;
use crate::tab_bar::{
    ScrollbarController, ScrollbarPointerUpdate, TabBarAction, TabBarController,
    TabBarPointerUpdate, TabCommandStatus, TabData,
};
use crate::{input, renderer};

static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

/// State owned by one terminal tab.
pub(crate) struct TerminalTab {
    id: TabId,
    title: String,
    command_status: TabCommandStatus,
    message_buffer: MessageBuffer,
    event_queue: Vec<WinitEvent<Event>>,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    inline_search_state: InlineSearchState,
    search_state: SearchState,
    notifier: Notifier,
    event_proxy: EventProxy,
    mouse: Mouse,
    touch: TouchPurpose,
    preserve_title: bool,
    hold: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
}

impl TerminalTab {
    fn new(
        display: &Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let id = TabId(NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed));
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let preserve_title = options.window_identity.title.is_some();
        let hold = options.terminal_options.hold;
        let title = options
            .window_identity
            .title
            .clone()
            .unwrap_or_else(|| config.window.identity.title.clone());

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let event_proxy = EventProxy::new(proxy, display.window.id(), id);
        let terminal = Term::new(config.term_options(), &display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty = tty::new(&pty_config, display.size_info.into(), display.window.id().into())?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
        )?;
        let loop_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        Ok(Self {
            id,
            title,
            command_status: TabCommandStatus::None,
            preserve_title,
            hold,
            terminal,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            config,
            event_proxy,
            notifier: Notifier(loop_tx),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            inline_search_state: Default::default(),
            message_buffer: Default::default(),
            window_config: Default::default(),
            search_state: Default::default(),
            event_queue: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
        })
    }

    fn update_config(&mut self, config: Rc<UiConfig>) {
        let old_config = self.config.clone();
        self.config = self.window_config.override_config_rc(config);
        self.terminal.lock().set_options(self.config.term_options());

        if !self.preserve_title
            && (!self.config.window.dynamic_title || self.title == old_config.window.identity.title)
        {
            self.title = self.config.window.identity.title.clone();
        }
    }

    fn retarget_window(&mut self, window_id: WindowId) {
        self.event_proxy.set_window_id(window_id);
        self.event_queue.retain_mut(|queued_event| {
            let WinitEvent::UserEvent(event) = queued_event else {
                return false;
            };

            event.window_id = Some(window_id);
            true
        });
    }
}

impl Drop for TerminalTab {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

/// Event context for one individual Alacritty native window.
pub struct WindowContext {
    pub display: Display,
    pub dirty: bool,
    tabs: Vec<TerminalTab>,
    active_tab: usize,
    selected_tabs: HashSet<TabId>,
    tab_bar: TabBarController,
    tab_drop: Option<TabDrop>,
    tab_bar_shift: bool,
    scrollbar: ScrollbarController,
    modifiers: Modifiers,
    occluded: bool,
    focused: bool,
    pub(crate) closing: bool,
    #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
    rendered_frames: u64,
}

impl WindowContext {
    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;
        let display = Display::new(window, gl_context, &config, false)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let display = Self::additional_display(gl_config, event_loop, &config, &mut options, None)?;

        let mut window_context = Self::new(display, config, options, proxy)?;
        window_context.tabs[0].window_config = config_overrides;

        Ok(window_context)
    }

    /// Prepare the destination before removing any live terminals from the source.
    pub(crate) fn additional_for_tabs(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        config: &UiConfig,
        size: PhysicalSize<u32>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut options = WindowOptions::default();
        let display =
            Self::additional_display(gl_config, event_loop, config, &mut options, Some(size))?;
        Ok(Self::with_display(display, Vec::new())?)
    }

    fn additional_display(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        config: &UiConfig,
        options: &mut WindowOptions,
        requested_size: Option<PhysicalSize<u32>>,
    ) -> Result<Display, Box<dyn Error>> {
        let gl_display = gl_config.display();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        let window = Window::new(
            event_loop,
            config,
            &identity,
            options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        if let Some(size) = requested_size {
            let _ = window.request_inner_size(size);
        }

        let raw_window_handle = window.raw_window_handle();
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, gl_config, Some(raw_window_handle))?;
        Display::new(window, gl_context, config, false).map_err(Into::into)
    }

    fn new(
        display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let tab = TerminalTab::new(&display, config, options, proxy)?;
        Ok(Self::with_display(display, vec![tab])?)
    }

    fn with_display(
        display: Display,
        tabs: Vec<TerminalTab>,
    ) -> Result<Self, slint::PlatformError> {
        let mut tab_bar = TabBarController::new()?;
        let mut scrollbar = ScrollbarController::new()?;

        let size = display.window.inner_size();
        let scale_factor = display.window.scale_factor as f32;
        tab_bar.resize(size.width, scale_factor);
        scrollbar.resize(size.width, size.height, scale_factor);

        let window_context = Self {
            display,
            tabs,
            active_tab: 0,
            selected_tabs: HashSet::new(),
            tab_bar,
            tab_drop: None,
            tab_bar_shift: false,
            scrollbar,
            modifiers: Default::default(),
            occluded: Default::default(),
            focused: true,
            closing: false,
            dirty: true,
            #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
            rendered_frames: 0,
        };
        window_context.sync_tab_bar();
        Ok(window_context)
    }

    /// Create a terminal tab in this native window.
    pub fn create_tab(
        &mut self,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<(), Box<dyn Error>> {
        #[cfg(not(windows))]
        if options.terminal_options.working_directory.is_none() {
            let active = &self.tabs[self.active_tab];
            options.terminal_options.working_directory =
                crate::daemon::foreground_process_path(active.master_fd, active.shell_pid).ok();
        }

        self.append_tab(config, options, config_overrides, proxy)?;
        self.select_tab(self.tabs.len() - 1);
        Ok(())
    }

    fn append_tab(
        &mut self,
        config: Rc<UiConfig>,
        options: WindowOptions,
        config_overrides: ParsedOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<(), Box<dyn Error>> {
        let mut tab = TerminalTab::new(&self.display, config, options, proxy)?;
        tab.window_config = config_overrides;
        self.tabs.push(tab);
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn restore_tabs(
        &mut self,
        directories: impl Iterator<Item = Option<PathBuf>>,
        active_tab: usize,
        proxy: &EventLoopProxy<Event>,
    ) {
        let config = self.tabs[0].config.clone();
        let mut selected_tab = 0;
        // The initial window has already restored the first tab.
        for (index, working_directory) in directories.enumerate() {
            let mut options = WindowOptions::default();
            options.terminal_options.working_directory = working_directory;
            match self.append_tab(config.clone(), options, Default::default(), proxy.clone()) {
                Ok(()) if index + 1 == active_tab => selected_tab = self.tabs.len() - 1,
                Ok(()) => (),
                Err(err) => log::warn!("Unable to restore terminal tab: {err}"),
            }
        }
        if selected_tab == 0 {
            self.sync_tab_bar();
        } else {
            self.select_tab(selected_tab);
        }
    }

    #[cfg(unix)]
    pub(crate) fn session(&self) -> WindowSession {
        let directories = self
            .tabs
            .iter()
            .map(|tab| crate::daemon::process_path(tab.shell_pid as libc::pid_t).ok());
        WindowSession::new(directories, self.active_tab)
    }

    #[cfg(unix)]
    pub(crate) fn is_exiting_last_tab(&self, tab_id: TabId) -> bool {
        self.tabs.len() == 1 && self.tabs[0].id == tab_id && !self.tabs[0].hold
    }

    /// Apply a reloaded base configuration to every tab.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let active = self.active_tab;
        let old_config = self.tabs[active].config.clone();
        let window_id = self.id();

        for tab in &mut self.tabs {
            tab.update_config(new_config.clone());
            tab.event_queue.push(
                Event::new_for_tab(TerminalEvent::CursorBlinkingChange.into(), window_id, tab.id)
                    .into(),
            );
        }

        let config = self.tabs[active].config.clone();
        self.apply_display_config(&old_config, &config);
        self.display.window.set_title(self.tabs[active].title.clone());
        self.sync_tab_bar();
        self.dirty = true;
    }

    fn apply_display_config(&mut self, old_config: &UiConfig, config: &UiConfig) {
        self.display.update_config(config);

        if (old_config.cursor.thickness() - config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            self.display.font_size = config.font.size().scale(scale_factor);
            self.display
                .pending_update
                .set_font(config.font.clone().with_size(self.display.font_size));
        }

        let old_window = &old_config.window;
        if old_window.padding(1.) != config.window.padding(1.)
            || old_window.dynamic_padding != config.window.dynamic_padding
            || old_window.resize_increments != config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        self.display.window.set_theme(config.window.theme());
        let opaque = config.window_opacity() >= 1.;

        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);
        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(config.window.option_as_alt());

        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(config.window.blur);
        self.display.hint_state.update_alphabet(config.hints.alphabet());
        self.display.pending_update.dirty = true;
    }

    /// Get reference to the active tab's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.tabs[self.active_tab].config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        for tab in &mut self.tabs {
            tab.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);
            tab.window_config.clear();
        }
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        for tab in &mut self.tabs {
            tab.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);
            tab.window_config.extend_from_slice(options);
        }
        self.update_config(config);
    }

    /// Remove config-reload messages from every tab.
    pub fn clear_config_messages(&mut self) -> bool {
        let mut changed = false;
        for tab in &mut self.tabs {
            if !tab.message_buffer.is_empty() {
                tab.message_buffer.remove_target(crate::logging::LOG_TARGET_CONFIG);
                changed = true;
            }
        }
        changed
    }

    /// Draw the active terminal and the Slint tab strip.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            return;
        }

        self.dirty = false;
        self.display.process_renderer_update();

        if !self.display.visual_bell.completed() {
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        let tab = &mut self.tabs[self.active_tab];
        let terminal = tab.terminal.lock();
        self.display.draw(
            terminal,
            scheduler,
            &tab.message_buffer,
            &tab.config,
            &mut tab.search_state,
            &mut self.tab_bar,
            &mut self.scrollbar,
        );
        #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
        {
            self.rendered_frames += 1;
        }
    }

    /// Apply debug commands without duplicating tab activation or resize handling.
    #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
    pub(crate) fn ui_debug_action(
        &mut self,
        action: crate::ui_debug::UiDebugAction,
    ) -> Result<(), String> {
        use crate::ui_debug::UiDebugAction;

        match action {
            UiDebugAction::State => (),
            UiDebugAction::SelectTab { index } => {
                if index >= self.tabs.len() {
                    return Err(format!(
                        "tab index {index} out of range ({} tabs)",
                        self.tabs.len()
                    ));
                }
                self.select_tab(index);
            },
            UiDebugAction::Output { data } => {
                let mut terminal = self.tabs[self.active_tab].terminal.lock();
                let mut parser = alacritty_terminal::vte::ansi::Processor::<
                    alacritty_terminal::vte::ansi::StdSyncHandler,
                >::new();
                parser.advance(&mut *terminal, data.as_bytes());
                self.dirty = true;
                self.display.window.request_redraw();
            },
            UiDebugAction::CreateTab
            | UiDebugAction::Resize { .. }
            | UiDebugAction::Redraw
            | UiDebugAction::TabPointer { .. }
            | UiDebugAction::TabDrop { .. } => {
                unreachable!("handled by the application event loop")
            },
        }

        Ok(())
    }

    /// Snapshot the real terminal grids without synchronizing or repairing their geometry.
    #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
    pub(crate) fn ui_debug_state(&self) -> crate::ui_debug::WindowState {
        use crate::ui_debug::{TabState, WindowState};
        use alacritty_terminal::index::{Column, Line};

        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                let terminal = tab.terminal.lock();
                let cursor = terminal.grid().cursor.point;
                TabState {
                    id: tab.id.0,
                    selected: self.selected_tabs.contains(&tab.id),
                    title: tab.title.clone(),
                    columns: terminal.columns(),
                    screen_lines: terminal.screen_lines(),
                    cursor_line: cursor.line.0,
                    cursor_column: cursor.column.0,
                    queued_events: tab.event_queue.len(),
                }
            })
            .collect();
        let terminal = self.tabs[self.active_tab].terminal.lock();
        let offset = terminal.grid().display_offset() as i32;
        let text = (0..terminal.screen_lines())
            .map(|row| {
                let line = Line(row as i32 - offset);
                let text: String = (0..terminal.columns())
                    .map(|col| terminal.grid()[line][Column(col)].c)
                    .collect();
                text.trim_end().to_owned()
            })
            .collect();
        let size = self.display.window.inner_size();
        let surface_size = self.display.ui_debug_surface_size();
        #[cfg(all(feature = "x11", not(target_os = "macos")))]
        let native_position = self.display.window.inner_position().map(|point| (point.x, point.y));
        #[cfg(not(all(feature = "x11", not(target_os = "macos"))))]
        let native_position = None;
        WindowState {
            window_id: self.id().into(),
            active_tab: self.active_tab,
            focused: self.focused,
            native_position,
            native_width: size.width,
            native_height: size.height,
            surface_width: surface_size.width,
            surface_height: surface_size.height,
            columns: self.display.size_info.columns(),
            screen_lines: self.display.size_info.screen_lines(),
            rendered_frames: self.rendered_frames,
            occluded: self.occluded,
            tabs,
            text,
        }
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        let event = match self.preprocess_window_event(event, event_proxy, scheduler) {
            Some(event) => event,
            None => return,
        };

        let target_tab = match &event {
            WinitEvent::UserEvent(event) => event.tab_id,
            _ => None,
        };
        let target_index = match target_tab {
            Some(tab_id) => {
                let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                    return;
                };
                index
            },
            None => self.active_tab,
        };

        match event {
            // Titles are visible even for tabs whose input and rendering events are queued.
            WinitEvent::UserEvent(Event {
                payload: EventType::Terminal(TerminalEvent::Title(title)),
                ..
            }) => return self.update_tab_title(target_index, Some(title)),
            WinitEvent::UserEvent(Event {
                payload: EventType::Terminal(TerminalEvent::ResetTitle),
                ..
            }) => return self.update_tab_title(target_index, None),
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                if self.tabs[self.active_tab].event_queue.is_empty() {
                    return;
                }
            },
            event => {
                self.tabs[target_index].event_queue.push(event);
                return;
            },
        }

        // Background tabs retain their staged events until selected.
        let events = mem::take(&mut self.tabs[self.active_tab].event_queue);

        // Tab activation can change both the active terminal and its geometry. Apply that update
        // before dispatching queued input, since mouse points are calculated from `size_info` and
        // must address the same grid dimensions.
        self.flush_active_display_update();

        let tab = &mut self.tabs[self.active_tab];
        let mut terminal = tab.terminal.lock();
        let old_is_searching = tab.search_state.history_index.is_some();

        let context = ActionContext {
            cursor_blink_timed_out: &mut tab.cursor_blink_timed_out,
            prev_bell_cmd: &mut tab.prev_bell_cmd,
            message_buffer: &mut tab.message_buffer,
            inline_search_state: &mut tab.inline_search_state,
            search_state: &mut tab.search_state,
            modifiers: &mut self.modifiers,
            notifier: &mut tab.notifier,
            display: &mut self.display,
            mouse: &mut tab.mouse,
            touch: &mut tab.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: tab.master_fd,
            #[cfg(not(windows))]
            shell_pid: tab.shell_pid,
            tab_id: tab.id,
            hold: &mut tab.hold,
            config: &tab.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
        };
        let mut processor = input::Processor::new(context);

        for event in events {
            processor.handle_event(event);
        }

        if self.display.pending_update.dirty {
            Self::submit_display_update(
                &mut terminal,
                &mut self.display,
                &mut tab.notifier,
                &tab.message_buffer,
                &mut tab.search_state,
                old_is_searching,
                &tab.config,
            );
            self.tab_bar.resize(
                self.display.window.inner_size().width,
                self.display.window.scale_factor as f32,
            );
            let size = self.display.window.inner_size();
            self.scrollbar.resize(size.width, size.height, self.display.window.scale_factor as f32);
            self.dirty = true;
        }

        if self.dirty || tab.mouse.hint_highlight_dirty {
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                &tab.config,
                &tab.mouse,
                self.modifiers.state(),
            );
            tab.mouse.hint_highlight_dirty = false;
        }

        drop(terminal);
        self.sync_tab_bar();

        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    fn preprocess_window_event(
        &mut self,
        event: WinitEvent<Event>,
        event_proxy: &EventLoopProxy<Event>,
        scheduler: &mut Scheduler,
    ) -> Option<WinitEvent<Event>> {
        let WinitEvent::WindowEvent { window_id, event } = event else {
            return Some(event);
        };

        let scale_factor = self.display.window.scale_factor;
        let tab_height = TabBarController::physical_height(scale_factor as f32) as f64;

        let event = match event {
            WindowEvent::CloseRequested => {
                if !self.closing {
                    self.closing = true;
                    for tab in &mut self.tabs {
                        tab.hold = false;
                        tab.terminal.lock().exit();
                    }
                }
                return None;
            },
            WindowEvent::CursorMoved { device_id, mut position } => {
                let window_size = self.display.window.inner_size();
                let pointer_inside_window = position.x >= 0.
                    && position.y >= 0.
                    && position.x < window_size.width as f64
                    && position.y < window_size.height as f64;
                self.tab_bar.set_pointer_inside_window(pointer_inside_window);
                let logical = position.to_logical::<f32>(scale_factor);
                let logical_position = LogicalPosition::new(logical.x, logical.y);
                let tab_update =
                    self.tab_bar.move_pointer(LogicalPosition::new(logical.x, logical.y));
                let scrollbar_update = self.scrollbar.move_pointer(logical_position);
                let tab_captured = tab_update.captured;
                let scrollbar_captured = scrollbar_update.captured;
                let ui_captured = position.y < tab_height || tab_captured || scrollbar_captured;
                let cursor = ui_captured.then_some(
                    if tab_update.pointer_cursor || scrollbar_update.pointer_cursor {
                        CursorIcon::Pointer
                    } else {
                        CursorIcon::Default
                    },
                );
                self.display.window.set_mouse_visible(true);
                // Terminal input owns the cursor outside the tab bar and scrollbar. Clearing
                // the UI override also restores it when returning to the same terminal cell.
                self.display.window.set_ui_mouse_cursor(cursor);
                self.handle_tab_bar_update(tab_update, event_proxy, scheduler);
                self.handle_scrollbar_update(scrollbar_update, event_proxy);

                if ui_captured {
                    return None;
                }
                position.y -= tab_height;
                WindowEvent::CursorMoved { device_id, position }
            },
            WindowEvent::MouseInput { device_id, state, button } => {
                if button == MouseButton::Left && state == ElementState::Pressed {
                    self.dismiss_tab_selection_on_pointer_press();
                }
                if self.tab_bar.contains_pointer() || self.tab_bar.captures_pointer() {
                    self.display.window.set_mouse_visible(true);
                    if button == MouseButton::Left {
                        let update = self
                            .tab_bar
                            .pointer_button(state == ElementState::Pressed, self.tab_bar_shift);
                        self.handle_tab_bar_update(update, event_proxy, scheduler);
                    }
                    return None;
                }
                if self.scrollbar.contains_pointer() || self.scrollbar.is_dragging() {
                    self.display.window.set_mouse_visible(true);
                    if button == MouseButton::Left {
                        let update = self.scrollbar.pointer_button(state == ElementState::Pressed);
                        self.handle_scrollbar_update(update, event_proxy);
                    }
                    return None;
                }
                WindowEvent::MouseInput { device_id, state, button }
            },
            WindowEvent::MouseWheel { device_id, delta, phase } => {
                if self.tab_bar.contains_pointer() || self.tab_bar.captures_pointer() {
                    return None;
                }
                WindowEvent::MouseWheel { device_id, delta, phase }
            },
            WindowEvent::CursorLeft { device_id } => {
                let tab_captured = self.tab_bar.captures_pointer();
                let redraw = self.tab_bar.pointer_exited() | self.scrollbar.pointer_exited();
                self.request_slint_redraw(redraw);
                if tab_captured {
                    return None;
                }
                WindowEvent::CursorLeft { device_id }
            },
            WindowEvent::Touch(mut touch) => {
                let window_size = self.display.window.inner_size();
                let pointer_inside_window = touch.location.x >= 0.
                    && touch.location.y >= 0.
                    && touch.location.x < window_size.width as f64
                    && touch.location.y < window_size.height as f64;
                self.tab_bar.set_pointer_inside_window(pointer_inside_window);
                let logical = touch.location.to_logical::<f32>(scale_factor);
                let logical_position = LogicalPosition::new(logical.x, logical.y);
                let tab_update = self.tab_bar.move_pointer(logical_position);
                let mut scrollbar_update = self.scrollbar.move_pointer(logical_position);
                let tab_captured = tab_update.captured;
                self.handle_tab_bar_update(tab_update, event_proxy, scheduler);
                if touch.phase == TouchPhase::Started {
                    self.dismiss_tab_selection_on_pointer_press();
                }
                if touch.location.y < tab_height || tab_captured {
                    let update = match touch.phase {
                        TouchPhase::Started => self.tab_bar.pointer_button(true, false),
                        TouchPhase::Ended => self.tab_bar.pointer_button(false, false),
                        TouchPhase::Cancelled => {
                            let redraw = self.tab_bar.cancel_pointer_press();
                            TabBarPointerUpdate { redraw, ..Default::default() }
                        },
                        TouchPhase::Moved => TabBarPointerUpdate::default(),
                    };
                    self.handle_tab_bar_update(update, event_proxy, scheduler);
                    return None;
                }
                if scrollbar_update.captured {
                    let movement_delta = scrollbar_update.scroll_delta;
                    let movement_redraw = scrollbar_update.redraw;
                    match touch.phase {
                        TouchPhase::Started => {
                            scrollbar_update = self.scrollbar.pointer_button(true);
                            scrollbar_update.scroll_delta += movement_delta;
                            scrollbar_update.redraw |= movement_redraw;
                        },
                        TouchPhase::Ended => {
                            scrollbar_update = self.scrollbar.pointer_button(false);
                            scrollbar_update.scroll_delta += movement_delta;
                            scrollbar_update.redraw |= movement_redraw;
                        },
                        TouchPhase::Cancelled => {
                            scrollbar_update.redraw |= self.scrollbar.pointer_exited();
                        },
                        TouchPhase::Moved => (),
                    }
                    self.handle_scrollbar_update(scrollbar_update, event_proxy);
                    return None;
                }
                self.handle_scrollbar_update(scrollbar_update, event_proxy);
                touch.location.y -= tab_height;
                WindowEvent::Touch(touch)
            },
            WindowEvent::Resized(size) => {
                let scale_factor = self.display.window.scale_factor as f32;
                self.tab_bar.resize(size.width, scale_factor);
                self.scrollbar.resize(size.width, size.height, scale_factor);
                WindowEvent::Resized(size)
            },
            WindowEvent::ScaleFactorChanged { scale_factor, inner_size_writer } => {
                let size = self.display.window.inner_size();
                self.tab_bar.resize(size.width, scale_factor as f32);
                self.scrollbar.resize(size.width, size.height, scale_factor as f32);
                WindowEvent::ScaleFactorChanged { scale_factor, inner_size_writer }
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                self.tab_bar_shift = modifiers.state().shift_key();
                WindowEvent::ModifiersChanged(modifiers)
            },
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.tab_bar.set_active(focused);
                self.scrollbar.set_active(focused);
                WindowEvent::Focused(focused)
            },
            event => event,
        };

        Some(WinitEvent::WindowEvent { window_id, event })
    }

    fn handle_scrollbar_update(
        &mut self,
        update: ScrollbarPointerUpdate,
        event_proxy: &EventLoopProxy<Event>,
    ) {
        self.request_slint_redraw(update.redraw);
        if update.scroll_delta == 0 {
            return;
        }

        let tab_id = self.tabs[self.active_tab].id;
        let action = EventType::Scroll(Scroll::Delta(update.scroll_delta));
        let event = Event::new_for_tab(action, self.id(), tab_id);
        let _ = event_proxy.send_event(event);
    }

    fn handle_tab_bar_update(
        &mut self,
        update: TabBarPointerUpdate,
        event_proxy: &EventLoopProxy<Event>,
        scheduler: &mut Scheduler,
    ) {
        self.request_slint_redraw(update.redraw);
        if let Some(action) = update.action {
            self.handle_tab_bar_action(action, event_proxy, scheduler);
        }
    }

    fn request_slint_redraw(&mut self, redraw: bool) {
        if !redraw {
            return;
        }

        self.dirty = true;
        if self.display.window.has_frame && !self.occluded {
            self.display.window.request_redraw();
        }
    }

    fn handle_tab_bar_action(
        &mut self,
        action: TabBarAction,
        event_proxy: &EventLoopProxy<Event>,
        scheduler: &mut Scheduler,
    ) {
        match action {
            TabBarAction::Select(index) => self.select_tab(index),
            TabBarAction::SelectRange(index) => self.select_tab_range(index),
            TabBarAction::Close(index) if self.tabs.len() == 1 && index == 0 => {
                self.tabs[0].hold = false;
                self.tabs[0].terminal.lock().exit();
            },
            TabBarAction::Close(index) => {
                self.close_tab(index, scheduler);
            },
            TabBarAction::Drop { index, inside, source_slot } => {
                if let Some(drag) = self.tab_drag(index) {
                    self.tab_drop = Some(TabDrop {
                        drag,
                        position: self.tab_bar.cursor_position(),
                        inside,
                        source_slot,
                    });
                }
            },
            TabBarAction::New => {
                let action = EventType::TabAction(TabAction::Create(WindowOptions::default()));
                let _ = event_proxy.send_event(Event::new(action, self.id()));
            },
        }
    }

    pub(crate) fn take_tab_drop(&mut self) -> Option<TabDrop> {
        self.tab_drop.take()
    }

    /// Handle a wakeup from a specific PTY.
    pub fn mark_tab_dirty(&mut self, tab_id: Option<TabId>) {
        if tab_id.is_none_or(|tab_id| self.tabs[self.active_tab].id == tab_id) {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    pub fn mark_command_finished(&mut self, tab_id: TabId, status: i32) {
        let Some((index, tab)) = self.tabs.iter_mut().enumerate().find(|(_, tab)| tab.id == tab_id)
        else {
            return;
        };
        if index == self.active_tab {
            return;
        }

        let status =
            if status == 0 { TabCommandStatus::Success } else { TabCommandStatus::Failure };
        if tab.command_status != status {
            tab.command_status = status;
            self.sync_tab_bar();
            self.request_slint_redraw(true);
        }
    }

    /// Apply a tab action. Returns true when the native window should close.
    pub fn handle_tab_action(
        &mut self,
        action: TabAction,
        config: Rc<UiConfig>,
        proxy: EventLoopProxy<Event>,
        scheduler: &mut Scheduler,
        global_overrides: &ParsedOptions,
    ) -> Result<bool, Box<dyn Error>> {
        match action {
            TabAction::Create(options) => {
                let mut config_overrides = options.config_overrides();
                config_overrides.extend_from_slice(global_overrides);
                let config = config_overrides.override_config_rc(config);
                self.create_tab(config, options, config_overrides, proxy)?;
            },
            TabAction::Close(index) => return Ok(self.close_tab(index, scheduler)),
            TabAction::Select(index) => self.select_tab(index),
            TabAction::SelectNext if !self.tabs.is_empty() => {
                self.select_tab((self.active_tab + 1) % self.tabs.len());
            },
            TabAction::SelectPrevious if !self.tabs.is_empty() => {
                self.select_tab((self.active_tab + self.tabs.len() - 1) % self.tabs.len());
            },
            TabAction::SelectLast if !self.tabs.is_empty() => self.select_tab(self.tabs.len() - 1),
            _ => (),
        }

        Ok(false)
    }

    /// Remove the tab owning a PTY which has exited.
    pub fn close_tab_by_id(&mut self, tab_id: TabId, scheduler: &mut Scheduler) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return false;
        };
        if self.tabs[index].hold {
            return false;
        }
        self.close_tab(index, scheduler)
    }

    pub(crate) fn contains_tab(&self, tab_id: TabId) -> bool {
        self.tabs.iter().any(|tab| tab.id == tab_id)
    }

    fn tab_drag(&self, index: usize) -> Option<TabDrag> {
        let tab_id = self.tabs.get(index)?.id;
        if self.selected_tabs.contains(&tab_id) {
            let tabs = self.tabs.iter().filter(|tab| self.selected_tabs.contains(&tab.id));
            Some(TabDrag {
                tabs: tabs.map(|tab| tab.id).collect(),
                active: self.tabs[self.active_tab].id,
            })
        } else {
            Some(TabDrag { tabs: vec![tab_id], active: tab_id })
        }
    }

    #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
    pub(crate) fn external_tab_drag(&self) -> Option<TabDrag> {
        if self.tab_bar.contains_pointer() {
            return None;
        }
        self.tab_drag(self.tab_bar.dragged_tab()?)
    }

    #[cfg(all(unix, feature = "ui-debug", debug_assertions))]
    pub(crate) fn cancel_tab_drag(&mut self) {
        let redraw = self.tab_bar.cancel_pointer_press();
        self.request_slint_redraw(redraw);
    }

    pub(crate) fn tab_drop_slot(&self, position: LogicalPosition) -> usize {
        self.tab_bar.drop_index(position).unwrap_or(self.tabs.len())
    }

    pub(crate) fn tab_config(&self, id: TabId) -> Option<Rc<UiConfig>> {
        self.tabs.iter().find(|tab| tab.id == id).map(|tab| tab.config.clone())
    }

    pub(crate) fn take_moved_tabs(
        &mut self,
        drag: &TabDrag,
        scheduler: &mut Scheduler,
    ) -> Vec<TerminalTab> {
        let old_config = self.tabs[self.active_tab].config.clone();
        let active_id = self.tabs[self.active_tab].id;
        let fallback =
            self.tabs[..self.active_tab].iter().filter(|tab| !drag.tabs.contains(&tab.id)).count();
        let mut moved = Vec::new();
        let window_id = self.id();
        for tab in mem::take(&mut self.tabs) {
            if drag.tabs.contains(&tab.id) {
                if tab.id == active_id {
                    tab.terminal.lock().is_focused = false;
                }
                scheduler.unschedule_tab(window_id, tab.id);
                self.selected_tabs.remove(&tab.id);
                moved.push(tab);
            } else {
                self.tabs.push(tab);
            }
        }

        if !self.tabs.is_empty() {
            self.active_tab = fallback.min(self.tabs.len() - 1);
            if self.tabs[self.active_tab].id == active_id {
                self.sync_tab_bar();
                self.request_slint_redraw(true);
            } else {
                self.activate_tab(self.active_tab, &old_config);
            }
        }
        moved
    }

    pub(crate) fn insert_moved_tabs(
        &mut self,
        mut tabs: Vec<TerminalTab>,
        active_id: TabId,
        slot: usize,
    ) {
        let active = tabs.iter().position(|tab| tab.id == active_id).unwrap();
        let old_config = if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.terminal.lock().is_focused = false;
            tab.config.clone()
        } else {
            tabs[active].config.clone()
        };
        let window_id = self.id();
        self.selected_tabs.clear();
        for tab in &mut tabs {
            tab.retarget_window(window_id);
            tab.mouse = Mouse::default();
            tab.touch = TouchPurpose::default();
        }
        if tabs.len() > 1 {
            self.selected_tabs.extend(tabs.iter().map(|tab| tab.id));
        }
        let slot = slot.min(self.tabs.len());
        self.tabs.splice(slot..slot, tabs);
        self.activate_tab(slot + active, &old_config);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    fn close_tab(&mut self, index: usize, scheduler: &mut Scheduler) -> bool {
        if index >= self.tabs.len() {
            return false;
        }

        let closing_active_tab = index == self.active_tab;
        let old_config = self.tabs[self.active_tab].config.clone();
        let tab_id = self.tabs[index].id;
        self.selected_tabs.remove(&tab_id);
        scheduler.unschedule_tab(self.id(), tab_id);
        self.tabs.remove(index);

        if self.tabs.is_empty() {
            return true;
        }

        if index < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }

        if closing_active_tab {
            self.selected_tabs.clear();
            self.activate_tab(self.active_tab, &old_config);
        } else {
            self.sync_tab_bar();
            self.dirty = true;
            if self.display.window.has_frame && !self.occluded {
                self.display.window.request_redraw();
            }
        }

        false
    }

    fn select_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let selection_changed = !self.selected_tabs.is_empty();
        self.selected_tabs.clear();
        if index == self.active_tab {
            if selection_changed {
                self.sync_tab_bar();
                self.request_slint_redraw(true);
            }
            return;
        }

        let old_config = self.tabs[self.active_tab].config.clone();
        self.tabs[self.active_tab].terminal.lock().is_focused = false;
        self.activate_tab(index, &old_config);
    }

    fn select_tab_range(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let start = self.active_tab.min(index);
        let end = self.active_tab.max(index);
        self.selected_tabs.clear();
        if start != end {
            self.selected_tabs.extend(self.tabs[start..=end].iter().map(|tab| tab.id));
        }
        self.sync_tab_bar();
        self.request_slint_redraw(true);
    }

    fn dismiss_tab_selection_on_pointer_press(&mut self) {
        if !self.selected_tabs.is_empty()
            && !self.tab_bar.captures_pointer()
            && !self.tab_bar.pointer_over_tab()
        {
            self.selected_tabs.clear();
            self.sync_tab_bar();
            self.request_slint_redraw(true);
        }
    }

    pub(crate) fn reorder_tabs(&mut self, drag: &TabDrag, slot: usize) {
        let active_id = self.tabs[self.active_tab].id;
        let slot = self.tabs[..slot.min(self.tabs.len())]
            .iter()
            .filter(|tab| !drag.tabs.contains(&tab.id))
            .count();
        let (moved, mut remaining): (Vec<_>, Vec<_>) =
            mem::take(&mut self.tabs).into_iter().partition(|tab| drag.tabs.contains(&tab.id));
        remaining.splice(slot..slot, moved);
        self.tabs = remaining;
        self.active_tab = self.tabs.iter().position(|tab| tab.id == active_id).unwrap();
        self.sync_tab_bar();
        self.request_slint_redraw(true);
    }

    fn activate_tab(&mut self, index: usize, old_config: &UiConfig) {
        self.active_tab = index;
        self.tabs[index].command_status = TabCommandStatus::None;
        self.tabs[self.active_tab].terminal.lock().is_focused = self.focused;

        let config = self.tabs[self.active_tab].config.clone();
        self.apply_display_config(old_config, &config);
        // A different terminal replaces the entire viewport, even at identical dimensions.
        self.display.damage_tracker.invalidate();
        self.flush_active_display_update();
        self.display.window.set_title(self.tabs[self.active_tab].title.clone());
        let window_id = self.id();
        let tab_id = self.tabs[self.active_tab].id;
        self.tabs[self.active_tab].event_queue.push(
            Event::new_for_tab(TerminalEvent::CursorBlinkingChange.into(), window_id, tab_id)
                .into(),
        );
        self.sync_tab_bar();
        self.dirty = true;
        if self.display.window.has_frame && !self.occluded {
            self.display.window.request_redraw();
        }
    }

    fn update_tab_title(&mut self, index: usize, title: Option<String>) {
        let tab = &mut self.tabs[index];
        if tab.preserve_title || !tab.config.window.dynamic_title {
            return;
        }

        let title = title.unwrap_or_else(|| tab.config.window.identity.title.clone());
        if tab.title == title {
            return;
        }

        tab.title = title;
        if index == self.active_tab {
            self.display.window.set_title(tab.title.clone());
        }
        self.sync_tab_bar();
        self.request_slint_redraw(true);
    }

    fn sync_tab_bar(&self) {
        self.tab_bar.set_tabs(self.tabs.iter().enumerate().map(|(index, tab)| TabData {
            title: tab.title.as_str().into(),
            active: index == self.active_tab,
            selected: self.selected_tabs.contains(&tab.id),
            command_status: tab.command_status,
        }));
    }

    /// Apply pending display geometry before rendering or processing the active tab's input queue.
    fn flush_active_display_update(&mut self) {
        if !self.display.pending_update.dirty {
            return;
        }

        let tab = &mut self.tabs[self.active_tab];
        let mut terminal = tab.terminal.lock();
        let old_is_searching = tab.search_state.history_index.is_some();
        Self::submit_display_update(
            &mut terminal,
            &mut self.display,
            &mut tab.notifier,
            &tab.message_buffer,
            &mut tab.search_state,
            old_is_searching,
            &tab.config,
        );

        let size = self.display.window.inner_size();
        let scale_factor = self.display.window.scale_factor as f32;
        self.tab_bar.resize(size.width, scale_factor);
        self.scrollbar.resize(size.width, size.height, scale_factor);
        self.dirty = true;
    }

    /// ID of this native window.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        let tab = &self.tabs[self.active_tab];
        let mut grid = tab.terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");
        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");
        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");
        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");
        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

    fn submit_display_update(
        terminal: &mut Term<EventProxy>,
        display: &mut Display,
        notifier: &mut Notifier,
        message_buffer: &MessageBuffer,
        search_state: &mut SearchState,
        old_is_searching: bool,
        config: &UiConfig,
    ) {
        let num_lines = terminal.screen_lines();
        let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
        let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point.line == num_lines - 1
        } else {
            search_state.direction == Direction::Left
        };

        display.handle_update(terminal, notifier, message_buffer, search_state, config);

        let new_is_searching = search_state.history_index.is_some();
        if !old_is_searching && new_is_searching {
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }
}
