use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use slint::platform::software_renderer::{PremultipliedRgbaColor, SoftwareRenderer};
use slint::platform::{Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, LogicalPosition, LogicalSize, Model, ModelRc, PhysicalSize, Rgb8Pixel,
    VecModel, Window, WindowSize,
};

slint::include_modules!();

/// Logical height of the tab strip.
pub const TAB_BAR_HEIGHT: f32 = 36.;

/// Logical width of the scrollbar overlay.
pub const SCROLLBAR_WIDTH: f32 = 12.;

const MIN_SCROLLBAR_THUMB_HEIGHT: f32 = 24.;

/// User actions emitted by the Slint tab strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabBarAction {
    Select(usize),
    Close(usize),
    New,
}

/// Result of moving a pointer over the tab strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabBarPointerUpdate {
    pub pointer_cursor: bool,
    pub redraw: bool,
}

/// Result of moving or clicking a pointer over the scrollbar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbarPointerUpdate {
    pub pointer_cursor: bool,
    pub redraw: bool,
    pub scroll_delta: i32,
    pub captured: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabBarHitTarget {
    Tab(usize),
    Close(usize),
    New,
    Background,
    Outside,
}

thread_local! {
    static PENDING_ADAPTERS: RefCell<VecDeque<Rc<SlintWindowAdapter>>> =
        const { RefCell::new(VecDeque::new()) };
}

/// Slint platform implementation which attaches components to Alacritty-owned windows.
struct AlacrittySlintPlatform;

impl Platform for AlacrittySlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        PENDING_ADAPTERS
            .with(|pending| {
                pending.borrow_mut().pop_front().map(|adapter| adapter as Rc<dyn WindowAdapter>)
            })
            .ok_or_else(|| PlatformError::Other("missing Alacritty window adapter".into()))
    }
}

/// Install the process-wide Slint platform before constructing any tab bars.
pub fn init_platform() -> Result<(), slint::platform::SetPlatformError> {
    slint::platform::set_platform(Box::new(AlacrittySlintPlatform))
}

/// Software-rendered Slint window attached to an Alacritty-owned native window.
struct SlintWindowAdapter {
    window: Window,
    renderer: SoftwareRenderer,
    size: Cell<PhysicalSize>,
    redraw_requested: Cell<bool>,
}

impl SlintWindowAdapter {
    fn new(width: u32, height: u32) -> Rc<Self> {
        Rc::<Self>::new_cyclic(|weak| Self {
            window: Window::new(weak.clone()),
            renderer: SoftwareRenderer::new(),
            size: Cell::new(PhysicalSize::new(width, height)),
            redraw_requested: Cell::new(true),
        })
    }

    fn resize(&self, width: u32, height: u32, scale_factor: f32) {
        let size = PhysicalSize::new(width, height);
        if self.window.scale_factor() != scale_factor {
            self.window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
        }
        if self.size.replace(size) != size {
            self.window
                .dispatch_event(WindowEvent::Resized { size: size.to_logical(scale_factor) });
        }
        self.redraw_requested.set(true);
    }
}

impl WindowAdapter for SlintWindowAdapter {
    fn window(&self) -> &Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn set_size(&self, size: WindowSize) {
        let physical_size = size.to_physical(self.window.scale_factor());
        self.resize(physical_size.width, physical_size.height, self.window.scale_factor());
    }

    fn request_redraw(&self) {
        self.redraw_requested.set(true);
    }

    fn set_visible(&self, _visible: bool) -> Result<(), PlatformError> {
        Ok(())
    }
}

/// State and software framebuffer for the Slint tab strip.
pub struct TabBarController {
    adapter: Rc<SlintWindowAdapter>,
    component: TabBar,
    model: Rc<VecModel<TabData>>,
    pixels: Vec<Rgb8Pixel>,
    cursor_position: LogicalPosition,
    hover_target: Cell<TabBarHitTarget>,
    pressed_target: Cell<Option<TabBarHitTarget>>,
}

impl TabBarController {
    pub fn new() -> Result<Self, slint::PlatformError> {
        let adapter = SlintWindowAdapter::new(800, TAB_BAR_HEIGHT as u32);
        PENDING_ADAPTERS.with(|pending| pending.borrow_mut().push_back(adapter.clone()));

        let component = TabBar::new()?;
        let model = Rc::new(VecModel::default());
        component.set_tabs(ModelRc::from(model.clone()));
        component.show()?;

        Ok(Self {
            adapter,
            component,
            model,
            pixels: Vec::new(),
            cursor_position: LogicalPosition::default(),
            hover_target: Cell::new(TabBarHitTarget::Outside),
            pressed_target: Cell::new(None),
        })
    }

    pub fn set_tabs(&self, titles: impl IntoIterator<Item = (String, bool)>) {
        let tabs = titles
            .into_iter()
            .map(|(title, active)| TabData { title: title.into(), active })
            .collect::<Vec<_>>();
        let unchanged = self.model.row_count() == tabs.len()
            && tabs.iter().enumerate().all(|(index, tab)| {
                self.model.row_data(index).is_some_and(|current| {
                    current.title == tab.title && current.active == tab.active
                })
            });
        if unchanged {
            return;
        }

        self.model.set_vec(tabs);
        self.component.set_tab_width(self.tab_width());
    }

    pub fn resize(&mut self, width: u32, scale_factor: f32) {
        let height = Self::physical_height(scale_factor);
        self.adapter.resize(width, height, scale_factor);
        self.component.set_tab_width(self.tab_width());
        self.pixels.resize(width as usize * height as usize, Rgb8Pixel::default());
    }

    pub fn physical_height(scale_factor: f32) -> u32 {
        (TAB_BAR_HEIGHT * scale_factor).round() as u32
    }

    pub fn render(&mut self) -> (&[Rgb8Pixel], u32, u32, bool) {
        slint::platform::update_timers_and_animations();

        let size = self.adapter.size.get();
        let expected_len = size.width as usize * size.height as usize;
        if self.pixels.len() != expected_len {
            self.pixels.resize(expected_len, Rgb8Pixel::default());
        }

        let redraw = self.adapter.redraw_requested.replace(false);
        if redraw {
            self.adapter.renderer.render(&mut self.pixels, size.width as usize);
        }

        (&self.pixels, size.width, size.height, redraw)
    }

    pub fn move_pointer(&mut self, position: LogicalPosition) -> TabBarPointerUpdate {
        self.cursor_position = position;
        let target = self.hit_target(position);
        let redraw = self.hover_target.replace(target) != target;

        if redraw {
            let hovered_tab = match target {
                TabBarHitTarget::Tab(index) | TabBarHitTarget::Close(index) => index as i32,
                _ => -1,
            };
            let hovered_close_tab = match target {
                TabBarHitTarget::Close(index) => index as i32,
                _ => -1,
            };
            self.component.set_hovered_tab(hovered_tab);
            self.component.set_hovered_close_tab(hovered_close_tab);
            self.component.set_new_tab_hovered(target == TabBarHitTarget::New);
        }

        TabBarPointerUpdate {
            pointer_cursor: matches!(target, TabBarHitTarget::Close(_) | TabBarHitTarget::New),
            redraw,
        }
    }

    pub fn pointer_button(&self, pressed: bool) -> Option<TabBarAction> {
        let target = self.hit_target(self.cursor_position);
        if pressed {
            self.pressed_target.set(Some(target));
            return None;
        }

        let pressed_target = self.pressed_target.take();
        if pressed_target != Some(target) {
            return None;
        }

        match target {
            TabBarHitTarget::Tab(index) => Some(TabBarAction::Select(index)),
            TabBarHitTarget::Close(index) => Some(TabBarAction::Close(index)),
            TabBarHitTarget::New => Some(TabBarAction::New),
            TabBarHitTarget::Background | TabBarHitTarget::Outside => None,
        }
    }

    pub fn cancel_pointer_press(&self) {
        self.pressed_target.set(None);
    }

    pub fn pointer_exited(&mut self) -> bool {
        self.pressed_target.set(None);
        self.move_pointer(LogicalPosition::new(-1., -1.)).redraw
    }

    pub fn set_active(&self, active: bool) {
        self.adapter.window.dispatch_event(WindowEvent::WindowActiveChanged(active));
    }

    pub fn logical_size(&self) -> LogicalSize {
        self.adapter.size.get().to_logical(self.adapter.window.scale_factor())
    }

    pub fn contains_pointer(&self) -> bool {
        self.hit_target(self.cursor_position) != TabBarHitTarget::Outside
    }

    fn tab_width(&self) -> f32 {
        let tab_count = self.model.row_count();
        if tab_count == 0 {
            return 96.;
        }

        ((self.logical_size().width - 40.) / tab_count as f32).clamp(96., 240.)
    }

    fn hit_target(&self, position: LogicalPosition) -> TabBarHitTarget {
        let size = self.logical_size();
        if position.x < 0.
            || position.y < 0.
            || position.x >= size.width
            || position.y >= size.height
        {
            return TabBarHitTarget::Outside;
        }

        let tab_count = self.model.row_count();
        let tab_width = self.tab_width();
        let tabs_end = tab_width * tab_count as f32;
        if position.x < tabs_end && tab_count > 0 {
            let index = (position.x / tab_width) as usize;
            let local_x = position.x - index as f32 * tab_width;
            let close_top = (TAB_BAR_HEIGHT - 24.) / 2.;
            if local_x >= tab_width - 30.
                && local_x < tab_width - 6.
                && position.y >= close_top
                && position.y < close_top + 24.
            {
                return TabBarHitTarget::Close(index);
            }
            return TabBarHitTarget::Tab(index);
        }

        if position.x >= (size.width - 40.).max(0.) {
            TabBarHitTarget::New
        } else {
            TabBarHitTarget::Background
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScrollbarState {
    display_offset: usize,
    history_size: usize,
    screen_lines: usize,
}

/// State and transparent software framebuffer for the Slint scrollbar overlay.
pub struct ScrollbarController {
    adapter: Rc<SlintWindowAdapter>,
    component: ScrollBarOverlay,
    pixels: Vec<PremultipliedRgbaColor>,
    state: ScrollbarState,
    surface_width: f32,
    cursor_position: LogicalPosition,
    hovered: bool,
    dragging: bool,
    drag_offset: f32,
}

impl ScrollbarController {
    pub fn new() -> Result<Self, slint::PlatformError> {
        let adapter = SlintWindowAdapter::new(SCROLLBAR_WIDTH as u32, 600 - TAB_BAR_HEIGHT as u32);
        PENDING_ADAPTERS.with(|pending| pending.borrow_mut().push_back(adapter.clone()));

        let component = ScrollBarOverlay::new()?;
        component.show()?;

        Ok(Self {
            adapter,
            component,
            pixels: Vec::new(),
            state: ScrollbarState::default(),
            surface_width: 800.,
            cursor_position: LogicalPosition::default(),
            hovered: false,
            dragging: false,
            drag_offset: 0.,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32, scale_factor: f32) {
        let scrollbar_width = (SCROLLBAR_WIDTH * scale_factor).round().max(1.) as u32;
        let tab_bar_height = TabBarController::physical_height(scale_factor);
        let scrollbar_height = height.saturating_sub(tab_bar_height).max(1);
        self.surface_width = width as f32 / scale_factor;
        self.adapter.resize(scrollbar_width, scrollbar_height, scale_factor);
        self.update_component_state();
        self.pixels.resize(
            scrollbar_width as usize * scrollbar_height as usize,
            PremultipliedRgbaColor::default(),
        );
    }

    pub fn set_state(&mut self, display_offset: usize, total_lines: usize, screen_lines: usize) {
        let state = ScrollbarState {
            display_offset,
            history_size: total_lines.saturating_sub(screen_lines),
            screen_lines,
        };
        if self.state == state {
            return;
        }

        self.state = state;
        self.update_component_state();
    }

    pub fn render(&mut self) -> (&[PremultipliedRgbaColor], u32, u32, bool, bool) {
        slint::platform::update_timers_and_animations();

        let size = self.adapter.size.get();
        let expected_len = size.width as usize * size.height as usize;
        if self.pixels.len() != expected_len {
            self.pixels.resize(expected_len, PremultipliedRgbaColor::default());
        }

        let redraw = self.adapter.redraw_requested.replace(false);
        if redraw {
            self.adapter.renderer.render(&mut self.pixels, size.width as usize);
        }

        (&self.pixels, size.width, size.height, redraw, self.visible())
    }

    pub fn move_pointer(&mut self, position: LogicalPosition) -> ScrollbarPointerUpdate {
        self.cursor_position = position;
        let hovered = self.hit_track(position);
        let mut update = ScrollbarPointerUpdate {
            pointer_cursor: hovered || self.dragging,
            captured: hovered || self.dragging,
            ..Default::default()
        };

        if self.hovered != hovered {
            self.hovered = hovered;
            self.component.set_hovered(hovered);
            update.redraw = true;
        }

        if self.dragging {
            update.scroll_delta = self.scroll_to_pointer();
            update.redraw |= update.scroll_delta != 0;
        }

        update
    }

    pub fn pointer_button(&mut self, pressed: bool) -> ScrollbarPointerUpdate {
        let was_dragging = self.dragging;
        let hovered = self.hit_track(self.cursor_position);
        let mut update = ScrollbarPointerUpdate {
            pointer_cursor: hovered || was_dragging,
            captured: hovered || was_dragging,
            ..Default::default()
        };

        if pressed && hovered {
            let local_y = self.local_y(self.cursor_position);
            let (thumb_y, thumb_height) = self.thumb_geometry();
            self.drag_offset = if local_y >= thumb_y && local_y < thumb_y + thumb_height {
                local_y - thumb_y
            } else {
                thumb_height / 2.
            };
            self.dragging = true;
            self.component.set_dragging(true);
            update.scroll_delta = self.scroll_to_pointer();
            update.redraw = true;
        } else if !pressed && was_dragging {
            self.dragging = false;
            self.component.set_dragging(false);
            update.redraw = true;
        }

        update.pointer_cursor = hovered || self.dragging;
        update
    }

    pub fn pointer_exited(&mut self) -> bool {
        let redraw = self.hovered || self.dragging;
        self.hovered = false;
        self.dragging = false;
        self.component.set_hovered(false);
        self.component.set_dragging(false);
        redraw
    }

    pub fn set_active(&self, active: bool) {
        self.adapter.window.dispatch_event(WindowEvent::WindowActiveChanged(active));
    }

    pub fn contains_pointer(&self) -> bool {
        self.hit_track(self.cursor_position)
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    fn visible(&self) -> bool {
        self.state.history_size > 0 && self.logical_size().height > 0.
    }

    fn logical_size(&self) -> LogicalSize {
        self.adapter.size.get().to_logical(self.adapter.window.scale_factor())
    }

    fn hit_track(&self, position: LogicalPosition) -> bool {
        let size = self.logical_size();
        self.visible()
            && position.x >= self.surface_width - SCROLLBAR_WIDTH
            && position.x < self.surface_width
            && position.y >= TAB_BAR_HEIGHT
            && position.y < TAB_BAR_HEIGHT + size.height
    }

    fn local_y(&self, position: LogicalPosition) -> f32 {
        position.y - TAB_BAR_HEIGHT
    }

    fn thumb_geometry(&self) -> (f32, f32) {
        let track_height = self.logical_size().height;
        if !self.visible() {
            return (0., track_height);
        }

        let total_lines = self.state.history_size + self.state.screen_lines;
        let thumb_height = (track_height * self.state.screen_lines as f32 / total_lines as f32)
            .max(MIN_SCROLLBAR_THUMB_HEIGHT)
            .min(track_height);
        let available_height = track_height - thumb_height;
        let scroll_fraction =
            self.state.display_offset as f32 / self.state.history_size.max(1) as f32;
        let thumb_y = available_height * (1. - scroll_fraction);
        (thumb_y, thumb_height)
    }

    fn scroll_to_pointer(&mut self) -> i32 {
        let (old_thumb_y, thumb_height) = self.thumb_geometry();
        let available_height = self.logical_size().height - thumb_height;
        if available_height <= 0. || self.state.history_size == 0 {
            return 0;
        }

        let thumb_y =
            (self.local_y(self.cursor_position) - self.drag_offset).clamp(0., available_height);
        let scroll_fraction = 1. - thumb_y / available_height;
        let display_offset = (scroll_fraction * self.state.history_size as f32).round() as usize;
        let delta = display_offset as i32 - self.state.display_offset as i32;
        if delta != 0 || (thumb_y - old_thumb_y).abs() > f32::EPSILON {
            self.state.display_offset = display_offset;
            self.component.set_thumb_y(thumb_y);
        }
        delta
    }

    fn update_component_state(&self) {
        let (thumb_y, thumb_height) = self.thumb_geometry();
        self.component.set_scrollbar_visible(self.visible());
        self.component.set_thumb_y(thumb_y);
        self.component.set_thumb_height(thumb_height);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs::File;
    use std::io::Write;

    use super::*;

    #[test]
    fn renders_preview() {
        init_platform().unwrap();

        let mut tab_bar = TabBarController::new().unwrap();
        tab_bar.set_tabs([
            ("shell — alacritty".to_owned(), true),
            ("cargo check".to_owned(), false),
            ("logs".to_owned(), false),
        ]);
        tab_bar.resize(800, 1.);

        let (pixels, width, height, updated) = tab_bar.render();
        let colors = pixels.iter().map(|pixel| (pixel.r, pixel.g, pixel.b)).collect::<HashSet<_>>();

        assert_eq!((width, height), (800, TAB_BAR_HEIGHT as u32));
        assert!(updated);
        assert!(colors.len() > 8, "tab strip should contain text and multiple UI colors");

        if let Some(path) = std::env::var_os("SLINT_TAB_BAR_SCREENSHOT") {
            let mut file = File::create(path).unwrap();
            write!(file, "P6\n{width} {height}\n255\n").unwrap();
            for pixel in pixels {
                file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
            }
        }
        assert!(!tab_bar.render().3, "unchanged tab strip should reuse its framebuffer");

        let update = tab_bar.move_pointer(LogicalPosition::new(250., 18.));
        assert!(!update.pointer_cursor);
        assert!(update.redraw);
        assert!(!tab_bar.move_pointer(LogicalPosition::new(251., 18.)).redraw);
        assert_eq!(click(&mut tab_bar, 250., 18.), Some(TabBarAction::Select(1)));

        assert!(tab_bar.move_pointer(LogicalPosition::new(226., 18.)).pointer_cursor);
        assert_eq!(click(&mut tab_bar, 226., 18.), Some(TabBarAction::Close(0)));

        tab_bar.resize(1_000, 1.);
        tab_bar.render();
        assert_eq!(click(&mut tab_bar, 980., 18.), Some(TabBarAction::New));

        assert!(tab_bar.move_pointer(LogicalPosition::new(980., 18.)).pointer_cursor);
        assert!(!tab_bar.move_pointer(LogicalPosition::new(500., 18.)).pointer_cursor);

        let mut scrollbar = ScrollbarController::new().unwrap();
        scrollbar.resize(800, 600, 1.);
        scrollbar.set_state(0, 240, 40);

        let (pixels, width, height, updated, visible) = scrollbar.render();
        assert_eq!((width, height), (SCROLLBAR_WIDTH as u32, 564));
        assert!(visible);
        assert!(updated);
        assert!(pixels.iter().any(|pixel| pixel.alpha == 0));
        assert!(pixels.iter().any(|pixel| pixel.alpha > 0));
        assert!(!scrollbar.render().3, "unchanged scrollbar should reuse its framebuffer");

        let (thumb_y, thumb_height) = scrollbar.thumb_geometry();
        let thumb_center = TAB_BAR_HEIGHT + thumb_y + thumb_height / 2.;
        let update = scrollbar.move_pointer(LogicalPosition::new(795., thumb_center));
        assert!(update.pointer_cursor);
        assert!(update.captured);
        assert!(update.redraw);
        assert!(!scrollbar.move_pointer(LogicalPosition::new(795., thumb_center + 1.)).redraw);

        let update = scrollbar.pointer_button(true);
        assert!(update.captured);
        assert_eq!(update.scroll_delta, 0);
        let update =
            scrollbar.move_pointer(LogicalPosition::new(795., TAB_BAR_HEIGHT + thumb_height / 2.));
        assert_eq!(update.scroll_delta, 200);
        assert!(scrollbar.is_dragging());
        let update = scrollbar.pointer_button(false);
        assert!(update.captured);
        assert!(!scrollbar.is_dragging());

        scrollbar.set_state(100, 240, 40);
        let update = scrollbar.move_pointer(LogicalPosition::new(795., 599.));
        assert!(update.captured);
        let update = scrollbar.pointer_button(true);
        assert_eq!(update.scroll_delta, -100);
        scrollbar.pointer_button(false);

        let update = scrollbar.move_pointer(LogicalPosition::new(700., 300.));
        assert!(!update.captured);
        assert!(!update.pointer_cursor);

        scrollbar.set_state(0, 40, 40);
        assert!(!scrollbar.render().4, "scrollbar should hide without scrollback");
    }

    fn click(tab_bar: &mut TabBarController, x: f32, y: f32) -> Option<TabBarAction> {
        tab_bar.move_pointer(LogicalPosition::new(x, y));
        assert_eq!(tab_bar.pointer_button(true), None);
        tab_bar.pointer_button(false)
    }
}
