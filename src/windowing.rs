//! Local window policy, independent of remote capture and video decoding.
use anyhow::{Context, Result};
use sdl2::{Sdl, VideoSubsystem, render::Canvas, video::Window};

pub fn init() -> Result<(Sdl, VideoSubsystem)> {
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
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg).context(
        "Could not initialize local window backend; check DISPLAY/Wayland or SDL_VIDEODRIVER",
    )?;
    Ok((sdl, video))
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
