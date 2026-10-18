//! Local window policy, independent of remote capture and video decoding.
use anyhow::{Context, Result};
use sdl2::{Sdl, VideoSubsystem, render::Canvas, video::Window};

pub fn init() -> Result<(Sdl, VideoSubsystem)> {
    // A click on a sibling monitor should both focus it and reach the desktop.
    sdl2::hint::set_with_priority(
        "SDL_MOUSE_FOCUS_CLICKTHROUGH",
        "1",
        &sdl2::hint::Hint::Override,
    );
    // Separate native fullscreen Spaces can hide sibling monitor windows.
    // Borderless desktop fullscreen keeps all mapped displays in one session.
    // SDL requires this hint before creating any windows (including launcher).
    #[cfg(target_os = "macos")]
    sdl2::hint::set_with_priority(
        "SDL_VIDEO_MAC_FULLSCREEN_SPACES",
        "0",
        &sdl2::hint::Hint::Override,
    );
    // Prefer the GPU-independent X11 presentation path when XWayland is
    // available. Hints are process-local (not inherited by child processes),
    // and avoid mutating the environment after Tokio starts its threads.
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty())
        && sdl2::hint::get("SDL_VIDEODRIVER").is_none()
        && sdl2::hint::get("SDL_VIDEO_DRIVER").is_none()
    {
        sdl2::hint::set("SDL_VIDEODRIVER", "x11");
    }
    // SDL's X11 GL contexts are GLX by default, which cannot import DMA-BUFs.
    // Opt in to EGL so Intel/AMD VA-API frames can be presented zero-copy
    // (sdl2-compat maps this to SDL3's SDL_VIDEO_FORCE_EGL). Must precede
    // SDL's GL library load. Wayland contexts are always EGL.
    #[cfg(target_os = "linux")]
    if std::env::var("TELEPORT_X11_EGL").as_deref() == Ok("1") {
        sdl2::hint::set("SDL_VIDEO_X11_FORCE_EGL", "1");
    }
    // sdl2-compat renders SDL2 programs at 1x on Wayland and lets the
    // compositor upscale them (blurry text and a half-size stream request on
    // HiDPI). Teleport draws at the drawable's own density, as on macOS.
    #[cfg(target_os = "linux")]
    if std::env::var_os("SDL_VIDEO_WAYLAND_SCALE_TO_DISPLAY").is_none() {
        sdl2::hint::set_with_priority(
            "SDL_VIDEO_WAYLAND_SCALE_TO_DISPLAY",
            "0",
            &sdl2::hint::Hint::Override,
        );
    }
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg).context(
        "Could not initialize local window backend; check DISPLAY/Wayland or SDL_VIDEODRIVER",
    )?;
    Ok((sdl, video))
}

/// The app icon for title bars, task switchers and docks that read the
/// window's own icon: 64x64 RGBA, rendered from
/// packaging/icons/dev.teleport.Teleport.svg.
const ICON_RGBA: &[u8; 64 * 64 * 4] = include_bytes!("../packaging/icons/window-64.rgba");

pub fn set_icon(window: &mut Window) {
    let mut pixels = *ICON_RGBA;
    // Bytes R, G, B, A in memory: ABGR8888 on little-endian, RGBA8888 on
    // big-endian; RGBA32 names that byte order on either.
    match sdl2::surface::Surface::from_data(
        &mut pixels,
        64,
        64,
        64 * 4,
        sdl2::pixels::PixelFormatEnum::RGBA32,
    ) {
        Ok(icon) => window.set_icon(icon),
        Err(error) => tracing::debug!(%error, "window icon unavailable"),
    }
}

pub fn with_icon(mut window: Window) -> Window {
    set_icon(&mut window);
    window
}

pub fn software(window: Window) -> Result<Canvas<Window>> {
    if window.subsystem().current_video_driver() == "x11" {
        // Only X11 has a CPU framebuffer path. Disabling this on Wayland
        // breaks presentation even with SDL's software renderer.
        sdl2::hint::set_with_priority(
            "SDL_FRAMEBUFFER_ACCELERATION",
            "0",
            &sdl2::hint::Hint::Override,
        );
    }
    // sdl2-compat lets SDL_RENDER_DRIVER override the SOFTWARE flag. An
    // explicit index keeps these UI windows genuinely software-rendered.
    let index = sdl2::render::drivers()
        .position(|driver| driver.name == "software")
        .context("SDL software renderer is unavailable")? as u32;
    let backend = window.subsystem().current_video_driver();
    window.into_canvas().index(index).software().build()
        .with_context(|| format!("Could not create software window renderer ({backend}); on Linux with XWayland try SDL_VIDEODRIVER=x11"))
}
