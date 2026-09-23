//! Fullscreen geometry and local-only, auto-hiding session chrome.
use anyhow::{Context, Result, ensure};
use sdl2::{
    VideoSubsystem,
    rect::Rect,
    video::{FullscreenType, Window, WindowPos},
};
use std::time::{Duration, Instant};

pub const HEIGHT: u32 = 82;
pub const REVEAL_HEIGHT: u32 = 4;
const HIDE_AFTER: Duration = Duration::from_millis(1200);

pub struct Chrome {
    fullscreen: bool,
    visible: bool,
    last_interaction: Instant,
}

impl Chrome {
    pub fn new() -> Self {
        Self {
            fullscreen: false,
            visible: true,
            last_interaction: Instant::now(),
        }
    }

    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
        self.visible = true;
        self.last_interaction = Instant::now();
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }
    pub fn visible(&self) -> bool {
        self.visible
    }
    pub fn inset(&self) -> u32 {
        if self.fullscreen { 0 } else { HEIGHT }
    }

    /// Pointer must belong to this window; `None` means focus/pointer elsewhere.
    /// Never hide while a button is held, to avoid converting a local toolbar
    /// gesture into a remote drag halfway through the gesture.
    pub fn update(&mut self, pointer: Option<(i32, i32)>, buttons_down: bool) -> bool {
        self.update_at(pointer, buttons_down, Instant::now())
    }

    fn update_at(&mut self, pointer: Option<(i32, i32)>, buttons_down: bool, now: Instant) -> bool {
        let was_visible = self.visible;
        if !self.fullscreen {
            self.visible = true;
        } else if pointer.is_some_and(|(_, y)| (0..REVEAL_HEIGHT as i32).contains(&y))
            || (self.visible && pointer.is_some_and(|(_, y)| (0..HEIGHT as i32).contains(&y)))
            || (self.visible && buttons_down)
        {
            self.visible = true;
            self.last_interaction = now;
        } else if now.saturating_duration_since(self.last_interaction) >= HIDE_AFTER {
            self.visible = false;
        }
        self.visible != was_visible
    }

    pub fn blocks_pointer(&self, y: i32) -> bool {
        y >= 0
            && y < if self.visible {
                HEIGHT as i32
            } else {
                REVEAL_HEIGHT as i32
            }
    }

    /// Fullscreen chrome overlays video: showing it never rescales the desktop
    /// or changes the coordinate system during a pointer gesture.
    pub fn rect(&self, window: (u32, u32), image: (u32, u32)) -> Rect {
        let inset = self.inset().min(window.1);
        let available = (window.0.max(1), window.1.saturating_sub(inset).max(1));
        let scale = (available.0 as f64 / image.0.max(1) as f64)
            .min(available.1 as f64 / image.1.max(1) as f64);
        let width = (image.0.max(1) as f64 * scale).round().max(1.0) as u32;
        let height = (image.1.max(1) as f64 * scale).round().max(1.0) as u32;
        Rect::new(
            (available.0.saturating_sub(width) / 2) as i32,
            (inset + available.1.saturating_sub(height) / 2) as i32,
            width,
            height,
        )
    }

    pub fn pointer(
        &self,
        x: i32,
        y: i32,
        window: (u32, u32),
        image: (u32, u32),
    ) -> Option<(f64, f64)> {
        if self.blocks_pointer(y) || image.0 == 0 || image.1 == 0 {
            return None;
        }
        let rect = self.rect(window, image);
        if !rect.contains_point((x, y)) {
            return None;
        }
        Some((
            (x - rect.x()) as f64 / rect.width() as f64,
            (y - rect.y()) as f64 / rect.height() as f64,
        ))
    }
}

/// Preserve actual windowed placement, including negative monitor coordinates.
pub struct Placement {
    position: (i32, i32),
    size: (u32, u32),
    maximized: bool,
}

impl Placement {
    pub fn capture(window: &Window) -> Self {
        Self {
            position: window.position(),
            size: window.size(),
            maximized: window.is_maximized(),
        }
    }

    pub fn restore(&self, window: &mut Window) -> Result<()> {
        window
            .set_fullscreen(FullscreenType::Off)
            .map_err(anyhow::Error::msg)?;
        window.restore();
        window.set_position(
            WindowPos::Positioned(self.position.0),
            WindowPos::Positioned(self.position.1),
        );
        window.set_size(self.size.0, self.size.1)?;
        if self.maximized {
            window.maximize();
        }
        Ok(())
    }
}

/// Use each display's current native mode. No resolution/refresh-rate mutation,
/// exclusive display capture, global pointer grab, or authentication changes.
pub fn enter(window: &mut Window, video: &VideoSubsystem, display: i32) -> Result<()> {
    let count = video.num_video_displays().map_err(anyhow::Error::msg)?;
    ensure!(
        (0..count).contains(&display),
        "local display is no longer connected"
    );
    let bounds = video.display_bounds(display).map_err(anyhow::Error::msg)?;
    window
        .set_fullscreen(FullscreenType::Off)
        .map_err(anyhow::Error::msg)?;
    window.set_position(
        WindowPos::Positioned(bounds.x()),
        WindowPos::Positioned(bounds.y()),
    );
    window.set_size(bounds.width(), bounds.height())?;
    window
        .set_fullscreen(FullscreenType::Desktop)
        .map_err(anyhow::Error::msg)
        .context("could not enter fullscreen on the selected local display")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_hides_reveals_and_stays_local() {
        let mut chrome = Chrome::new();
        chrome.set_fullscreen(true);
        let now = chrome.last_interaction;
        assert!(!chrome.update_at(Some((500, 500)), false, now + Duration::from_secs(1)));
        assert!(chrome.update_at(Some((500, 500)), false, now + HIDE_AFTER));
        assert!(!chrome.visible());
        assert!(!chrome.blocks_pointer(50));
        assert!(chrome.blocks_pointer(0));
        assert!(chrome.update_at(Some((500, 0)), false, now + Duration::from_secs(2)));
        assert!(chrome.blocks_pointer(50));
        assert!(!chrome.update_at(Some((500, 20)), false, now + Duration::from_secs(10)));
        assert!(!chrome.update_at(None, true, now + Duration::from_secs(20)));
        assert!(chrome.update_at(None, false, now + Duration::from_secs(22)));
        chrome.set_fullscreen(false);
        assert!(chrome.visible());
        assert!(!chrome.update_at(None, false, now + Duration::from_secs(100)));
    }

    #[test]
    fn fullscreen_geometry_does_not_shift_when_chrome_reveals() {
        let mut chrome = Chrome::new();
        let window = (1920, 1080);
        assert_eq!(chrome.inset(), HEIGHT);
        assert!(chrome.pointer(960, 50, window, window).is_none());
        chrome.set_fullscreen(true);
        assert_eq!(chrome.rect(window, window), Rect::new(0, 0, 1920, 1080));
        let center = chrome.pointer(960, 540, window, window);
        chrome.update_at(None, false, chrome.last_interaction + HIDE_AFTER);
        assert_eq!(center, Some((0.5, 0.5)));
        assert_eq!(chrome.pointer(960, 540, window, window), center);
        assert!(chrome.pointer(960, 50, window, window).is_some());
        assert!(chrome.pointer(-1, 500, window, window).is_none());
        assert!(chrome.pointer(1920, 500, window, window).is_none());
        assert!(chrome.pointer(960, 1080, window, window).is_none());
    }

    #[test]
    fn letterboxing_portrait_and_hidpi_use_logical_coordinates() {
        let mut chrome = Chrome::new();
        chrome.set_fullscreen(true);
        chrome.update_at(None, false, chrome.last_interaction + HIDE_AFTER);
        let rect = chrome.rect((1920, 1080), (720, 1280));
        assert_eq!(rect.height(), 1080);
        assert!(rect.x() > 0);
        assert!(chrome.pointer(0, 500, (1920, 1080), (720, 1280)).is_none());
        assert!(
            chrome
                .pointer(rect.x(), 500, (1920, 1080), (720, 1280))
                .is_some()
        );
        assert_eq!(
            chrome.pointer(720, 450, (1440, 900), (2880, 1800)),
            Some((0.5, 0.5))
        );
        assert!(chrome.pointer(20, 20, (100, 100), (0, 0)).is_none());
    }
}
