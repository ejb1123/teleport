//! Additional independently decoded remote monitors on local fullscreen displays.
use super::*;
use std::{ptr::NonNull, sync::Arc};

struct Texture(NonNull<sdl2::sys::SDL_Texture>);
impl Drop for Texture {
    fn drop(&mut self) {
        unsafe { sdl2::sys::SDL_DestroyTexture(self.0.as_ptr()) }
    }
}

struct Screen {
    // Textures and Metal views must be destroyed before the SDL renderer/window.
    texture: Option<Texture>,
    presenter: Option<crate::hdr_present::HdrPresenter>,
    canvas: sdl2::render::Canvas<sdl2::video::Window>,
    local: i32,
    remote: usize,
    desktop: Option<Desktop>,
    chrome: crate::fullscreen::Chrome,
    wheel: WheelAccumulator,
    pipeline: Option<media::Pipeline>,
    worker: Option<Network>,
    image: Option<media::LatestImage>,
    stats: Arc<crate::stats::StreamStats>,
    frame: Option<media::Image>,
    size: (u32, u32),
    ready: bool,
    dirty: bool,
    error: Option<String>,
    started: Instant,
    pointer: Option<(i32, i32)>,
    buttons: u32,
}

pub(super) struct Displays {
    screens: Vec<Screen>,
    remote: moq_net::broadcast::Consumer,
    options: Options,
    events: mpsc::Sender<Event>,
}

impl Displays {
    pub fn new(
        remote: moq_net::broadcast::Consumer,
        options: &Options,
        events: mpsc::Sender<Event>,
    ) -> Self {
        Self {
            screens: Vec::new(),
            remote,
            options: options.clone(),
            events,
        }
    }
    pub fn active(&self) -> bool {
        !self.screens.is_empty()
    }
    pub fn stopped(&mut self, index: usize, reason: String) {
        for screen in &mut self.screens {
            if screen.remote == index {
                if screen.error.is_none() {
                    screen.error = Some(reason.clone());
                }
                screen.ready = false;
                screen.worker = None;
                screen.pipeline = None;
                screen.image = None;
                screen.frame = None;
                screen.texture = None;
                screen.dirty = true;
            }
        }
    }
    pub fn stop(&mut self) {
        let _ = self.events.try_send(Event::ReleaseAll);
        for screen in self.screens.drain(..) {
            let _ = self.events.try_send(Event::MonitorStream {
                index: screen.remote,
                enabled: false,
            });
        }
    }
    pub fn start(
        &mut self,
        video: &sdl2::VideoSubsystem,
        primary: i32,
        desktop: &Desktop,
        runtime: &Runtime,
    ) -> Result<()> {
        self.stop();
        ensure!(
            desktop.multimonitor,
            "host does not support independent monitor streams"
        );
        let monitors: Vec<_> = (0..desktop.monitors.len())
            .filter(|i| *i != desktop.active_monitor)
            .collect();
        for (local, remote) in (0..video.num_video_displays().map_err(anyhow::Error::msg)?)
            .filter(|i| *i != primary)
            .zip(monitors)
            .take(4)
        {
            let window = video
                .window("Teleport - Additional Display", 1280, 720)
                .allow_highdpi()
                .resizable()
                .build()?;
            let mut canvas = if self.options.software_renderer {
                crate::windowing::software(window)?
            } else {
                window.into_canvas().accelerated().build()?
            };
            crate::fullscreen::enter(canvas.window_mut(), video, local)?;
            let mut chrome = crate::fullscreen::Chrome::new();
            chrome.set_fullscreen(true);
            let mut screen = Screen {
                texture: None,
                presenter: None,
                canvas,
                local,
                remote,
                desktop: None,
                chrome,
                wheel: WheelAccumulator::default(),
                pipeline: None,
                worker: None,
                image: None,
                stats: Arc::default(),
                frame: None,
                size: (0, 0),
                ready: false,
                dirty: true,
                error: None,
                started: Instant::now(),
                pointer: None,
                buttons: 0,
            };
            screen.start(&self.remote, &self.options, &self.events, runtime, desktop)?;
            self.screens.push(screen);
        }
        Ok(())
    }
    pub fn update(&mut self, index: usize, desktop: Desktop) -> Result<()> {
        protocol::validate_video_size(desktop.width, desktop.height)?;
        ensure!(
            desktop.version == protocol::VERSION
                && desktop.codec == self.options.codec
                && desktop.dynamic_range == self.options.dynamic_range
                && desktop.active_monitor == index,
            "invalid auxiliary stream metadata"
        );
        for screen in &mut self.screens {
            if screen.remote == index {
                screen.desktop = Some(desktop.clone());
                screen.ready = false;
                screen.dirty = true;
                screen.frame = None;
                screen.texture = None;
                screen.wheel = WheelAccumulator::default();
            }
        }
        Ok(())
    }
    /// Returns true for every auxiliary-window event, including local-only input.
    pub fn event(
        &mut self,
        event: &SdlEvent,
        desktop: &Desktop,
        runtime: &Runtime,
    ) -> Result<bool> {
        if matches!(
            event,
            SdlEvent::RenderTargetsReset { .. } | SdlEvent::RenderDeviceReset { .. }
        ) {
            for screen in &mut self.screens {
                screen.texture = None;
                screen.dirty = true;
            }
            return Ok(false);
        }
        if matches!(event, SdlEvent::Display { .. }) {
            self.stop();
            return Ok(false);
        }
        let Some(id) = event.get_window_id() else {
            return Ok(false);
        };
        let Some(position) = self
            .screens
            .iter()
            .position(|s| s.canvas.window().id() == id)
        else {
            return Ok(false);
        };
        let screen = &mut self.screens[position];
        screen.dirty = true;
        match *event {
            SdlEvent::Window {
                win_event: WindowEvent::Close,
                ..
            } => {
                self.stop();
                return Ok(true);
            }
            SdlEvent::Window {
                win_event: WindowEvent::FocusLost | WindowEvent::FocusGained,
                ..
            } => {
                screen.pointer = None;
                screen.buttons = 0;
                screen.wheel = WheelAccumulator::default();
                send(&self.events, Event::ReleaseAll)?;
            }
            SdlEvent::Window {
                win_event: WindowEvent::Leave,
                ..
            } => {
                screen.pointer = None;
            }
            SdlEvent::MouseMotion {
                x, y, mousestate, ..
            } => {
                screen.pointer = Some((x, y));
                screen.buttons = mousestate.to_sdl_state();
                screen.chrome.update(screen.pointer, screen.buttons != 0);
            }
            SdlEvent::MouseButtonDown { x, y, .. } => {
                screen.pointer = Some((x, y));
                screen.buttons = 1;
            }
            SdlEvent::MouseButtonUp { x, y, .. } => {
                screen.pointer = Some((x, y));
                screen.buttons = 0;
            }
            _ => (),
        }
        if let SdlEvent::MouseButtonDown {
            mouse_btn: MouseButton::Left,
            x,
            y,
            ..
        } = *event
            && screen.chrome.visible()
            && (0..TOOLBAR_HEIGHT as i32).contains(&y)
        {
            send(&self.events, Event::ReleaseAll)?;
            if x < 260 && desktop.monitors.len() > 1 && screen.ready {
                let old = screen.remote;
                let mappings: Vec<_> = self.screens.iter().map(|s| s.remote).collect();
                let shared = mapping_shared(&mappings, position);
                if let Some(next) = next_monitor(old, desktop.monitors.len()) {
                    let screen = &mut self.screens[position];
                    if !shared {
                        send(
                            &self.events,
                            Event::MonitorStream {
                                index: old,
                                enabled: false,
                            },
                        )?;
                    }
                    screen.remote = next;
                    screen.start(&self.remote, &self.options, &self.events, runtime, desktop)?;
                }
            } else if x >= 260 {
                self.stop();
            }
            return Ok(true);
        }
        let screen = &mut self.screens[position];
        if !screen.ready {
            return Ok(true);
        }
        let window = screen.canvas.window().size();
        let motion = |x, y| {
            screen
                .chrome
                .pointer(x, y, window, screen.size)
                .map(|(x, y)| Event::MonitorMotion {
                    index: screen.remote,
                    x,
                    y,
                })
        };
        match *event {
            SdlEvent::MouseMotion { x, y, .. } => {
                if let Some(event) = motion(x, y) {
                    send(&self.events, event)?;
                }
            }
            SdlEvent::MouseButtonDown {
                x, y, mouse_btn, ..
            }
            | SdlEvent::MouseButtonUp {
                x, y, mouse_btn, ..
            } => {
                if let Some(event_motion) = motion(x, y) {
                    send(&self.events, event_motion)?;
                    if let Some(button) = button(mouse_btn) {
                        send(
                            &self.events,
                            Event::Button {
                                button,
                                down: matches!(event, SdlEvent::MouseButtonDown { .. }),
                            },
                        )?;
                    }
                } else if matches!(event, SdlEvent::MouseButtonUp { .. }) {
                    send(&self.events, Event::ReleaseAll)?;
                }
            }
            SdlEvent::MouseWheel {
                x,
                y,
                precise_x,
                precise_y,
                direction,
                mouse_x,
                mouse_y,
                ..
            } => {
                let point = screen.chrome.pointer(mouse_x, mouse_y, window, screen.size);
                for event in screen.wheel.events(
                    (x, y),
                    (precise_x, precise_y),
                    direction == sdl2::mouse::MouseWheelDirection::Flipped,
                    point,
                )? {
                    send(
                        &self.events,
                        match event {
                            Event::Motion { x, y } => Event::MonitorMotion {
                                index: screen.remote,
                                x,
                                y,
                            },
                            other => other,
                        },
                    )?;
                }
            }
            SdlEvent::KeyDown {
                scancode: Some(code),
                repeat: false,
                ..
            } => {
                if let Some(code) = protocol::evdev(code) {
                    send(&self.events, Event::Key { code, down: true })?;
                }
            }
            SdlEvent::KeyUp {
                scancode: Some(code),
                ..
            } => {
                if let Some(code) = protocol::evdev(code) {
                    send(&self.events, Event::Key { code, down: false })?;
                }
            }
            _ => (),
        }
        Ok(true)
    }
    pub fn draw(&mut self, runtime: &Runtime, font: &sdl2::ttf::Font<'_, '_>) -> Result<()> {
        if self
            .screens
            .iter()
            .any(|screen| screen.canvas.window().display_index().is_err())
        {
            self.stop();
            return Ok(());
        }
        let mut failed = Vec::new();
        for screen in &mut self.screens {
            if let Err(error) = screen.draw(runtime, font) {
                tracing::warn!(monitor=screen.remote,%error,"additional monitor stopped");
                screen.error = Some(error.to_string());
                screen.ready = false;
                screen.worker = None;
                screen.pipeline = None;
                screen.image = None;
                screen.dirty = true;
                screen.frame = None;
                screen.texture = None;
                screen.chrome.set_fullscreen(true);
                failed.push(screen.remote);
            }
        }
        for index in failed {
            if !self
                .screens
                .iter()
                .any(|s| s.remote == index && s.worker.is_some())
            {
                let _ = self.events.try_send(Event::MonitorStream {
                    index,
                    enabled: false,
                });
            }
        }
        Ok(())
    }
}
impl Drop for Displays {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Screen {
    fn start(
        &mut self,
        remote: &moq_net::broadcast::Consumer,
        options: &Options,
        events: &mpsc::Sender<Event>,
        runtime: &Runtime,
        desktop: &Desktop,
    ) -> Result<()> {
        self.worker = None;
        self.pipeline = None;
        self.texture = None;
        self.presenter = None;
        self.frame = None;
        self.image = None;
        self.desktop = None;
        self.ready = false;
        self.error = None;
        self.started = Instant::now();
        self.stats = Arc::default();
        let native = cfg!(target_os = "macos")
            && std::env::var("TELEPORT_MAC_GPU_VIDEO").as_deref() == Ok("1")
            && !options.software_decoder
            && !options.software_renderer
            && desktop.dynamic_range == protocol::DynamicRange::Sdr;
        let nv12 = cfg!(target_os = "linux") && supports_nv12(&self.canvas);
        let (pipeline, source, image) = media::decoder_with_stats_presentation(
            options.software_decoder,
            self.stats.clone(),
            media::VideoFormat {
                codec: desktop.codec,
                dynamic_range: desktop.dynamic_range,
            },
            nv12,
            native,
        )?;
        self.presenter = if native {
            Some(crate::hdr_present::HdrPresenter::new_sdr(
                self.canvas.window(),
            )?)
        } else if desktop.dynamic_range == protocol::DynamicRange::Hdr10 {
            Some(crate::hdr_present::HdrPresenter::new(self.canvas.window())?)
        } else {
            None
        };
        let remote = remote.clone();
        let index = self.remote;
        let codec = desktop.codec;
        let stats = self.stats.clone();
        let output = events.clone();
        send(
            events,
            Event::MonitorStream {
                index,
                enabled: true,
            },
        )?;
        self.worker=Some(Network(runtime.spawn(async move {
            let track=tokio::time::timeout(Duration::from_secs(15),async { remote.track(&format!("monitor-{index}-{}",codec.track()))?.subscribe(media::video_subscription()).await.map_err(anyhow::Error::from) }).await.context("additional monitor subscription timed out")??;
            let (feedback,mut receive)=mpsc::channel(32);
            tokio::select! {
                result=media::receive_video_with_stats(track,source,feedback,stats)=>result,
                result=async { while let Some(event)=receive.recv().await { if let Event::Feedback{queue_ms,dropped_groups}=event { output.send(Event::MonitorFeedback{index,queue_ms,dropped_groups}).await?; } } Ok(()) }=>result,
            }
        })));
        self.pipeline = Some(pipeline);
        self.image = Some(image);
        self.dirty = true;
        Ok(())
    }
    fn draw(&mut self, runtime: &Runtime, font: &sdl2::ttf::Font<'_, '_>) -> Result<()> {
        if let Some(worker) = &mut self.worker {
            check_network(worker, runtime)?;
        }
        if let Some(pipeline) = &self.pipeline {
            pipeline.error()?;
        }
        if self.error.is_none() && !self.ready {
            ensure!(
                self.started.elapsed() < Duration::from_secs(20),
                "additional monitor produced no confirmed video"
            );
        }
        self.dirty |= self.chrome.update(self.pointer, self.buttons != 0);
        let frame = self.image.as_ref().and_then(|i| i.lock().unwrap().take());
        if let Some(frame) = frame
            && self.desktop.as_ref().is_some_and(|d| {
                video_generation_matches(d.video_start_group, frame.video_group)
                    && frame.width == d.width
                    && frame.height == d.height
            })
            && frame.decoder_recovery
                == self
                    .stats
                    .decoder_recoveries
                    .load(std::sync::atomic::Ordering::Relaxed)
        {
            protocol::validate_video_size(frame.width, frame.height)?;
            self.size = (frame.width, frame.height);
            self.ready = frame.surface.is_none() && frame.hdr.is_none();
            self.frame = Some(frame);
            self.dirty = true;
        }
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        let window = self.canvas.window().size();
        self.canvas
            .set_logical_size(window.0.max(1), window.1.max(1))?;
        self.canvas
            .set_draw_color(sdl2::pixels::Color::RGB(12, 15, 20));
        self.canvas.clear();
        let dest = self.chrome.rect(window, self.size);
        if let Some(frame) = &self.frame
            && frame.surface.is_none()
            && frame.hdr.is_none()
        {
            validate_frame(frame)?;
            // Reuse a renderer-owned streaming texture. Explicit RAII destroys it
            // before the canvas; no borrowed SDL texture escapes its creator.
            let format = if frame.nv12.is_some() {
                PixelFormatEnum::NV12
            } else {
                PixelFormatEnum::RGB24
            };
            unsafe {
                let mut old_format = 0;
                let mut width = 0;
                let mut height = 0;
                if let Some(texture) = &self.texture {
                    sdl2::sys::SDL_QueryTexture(
                        texture.0.as_ptr(),
                        &mut old_format,
                        std::ptr::null_mut(),
                        &mut width,
                        &mut height,
                    );
                }
                if old_format != format as u32
                    || width != frame.width as i32
                    || height != frame.height as i32
                {
                    set_nv12_conversion();
                    self.texture = Some(Texture(
                        NonNull::new(sdl2::sys::SDL_CreateTexture(
                            self.canvas.raw(),
                            format as u32,
                            sdl2::sys::SDL_TextureAccess::SDL_TEXTUREACCESS_STREAMING as i32,
                            frame.width as i32,
                            frame.height as i32,
                        ))
                        .context(sdl2::get_error())?,
                    ));
                }
                let texture = self.texture.as_ref().unwrap().0.as_ptr();
                let result = if let Some(nv) = &frame.nv12 {
                    sdl2::sys::SDL_UpdateNVTexture(
                        texture,
                        std::ptr::null(),
                        nv.y.as_ptr(),
                        nv.y_stride as i32,
                        nv.uv.as_ptr(),
                        nv.uv_stride as i32,
                    )
                } else {
                    sdl2::sys::SDL_UpdateTexture(
                        texture,
                        std::ptr::null(),
                        frame.data.as_ptr().cast(),
                        frame.stride as i32,
                    )
                };
                ensure!(
                    result == 0,
                    "auxiliary texture upload failed: {}",
                    sdl2::get_error()
                );
                ensure!(
                    sdl2::sys::SDL_RenderCopy(
                        self.canvas.raw(),
                        texture,
                        std::ptr::null(),
                        dest.raw()
                    ) == 0,
                    "auxiliary render failed: {}",
                    sdl2::get_error()
                );
            }
        }
        let reveal = Rect::new(
            window.0.saturating_sub(160) as i32 / 2,
            0,
            window.0.min(160),
            crate::fullscreen::REVEAL_HEIGHT,
        );
        if self.chrome.visible() {
            self.canvas
                .set_draw_color(sdl2::pixels::Color::RGB(21, 27, 38));
            self.canvas
                .fill_rect(Rect::new(0, 0, window.0, TOOLBAR_HEIGHT))
                .map_err(anyhow::Error::msg)?;
            let creator = self.canvas.texture_creator();
            let mut cache = std::collections::HashMap::new();
            session_text(
                &mut self.canvas,
                &creator,
                font,
                &mut cache,
                &format!(
                    "Local {} / Remote {} - Next",
                    self.local + 1,
                    self.remote + 1
                ),
                Rect::new(12, 12, 245, 32),
                sdl2::pixels::Color::WHITE,
            )?;
            session_text(
                &mut self.canvas,
                &creator,
                font,
                &mut cache,
                "Single display",
                Rect::new(270, 12, 200, 32),
                sdl2::pixels::Color::WHITE,
            )?;
            if let Some(error) = &self.error {
                session_text(
                    &mut self.canvas,
                    &creator,
                    font,
                    &mut cache,
                    error,
                    Rect::new(12, 48, window.0.saturating_sub(24), 26),
                    sdl2::pixels::Color::RGB(255, 160, 160),
                )?;
            }
        } else {
            self.canvas
                .set_draw_color(sdl2::pixels::Color::RGB(71, 196, 172));
            self.canvas.fill_rect(reveal).map_err(anyhow::Error::msg)?;
        }
        if let (Some(presenter), Some(frame)) = (&mut self.presenter, &self.frame) {
            let ui = Some(read_hdr_overlay(
                &mut self.canvas,
                if self.chrome.visible() {
                    Rect::new(0, 0, window.0, TOOLBAR_HEIGHT)
                } else {
                    reveal
                },
                window,
            )?);
            let overlays: Vec<_> = ui
                .iter()
                .map(|ui| crate::hdr_present::Overlay {
                    rect: ui.rect,
                    width: ui.width,
                    height: ui.height,
                    stride: ui.width as usize * 4,
                    rgba: &ui.pixels,
                })
                .collect();
            self.canvas.present();
            let outcome = if let Some(surface) = &frame.surface {
                presenter.present_surface(surface, dest, &overlays, window)?
            } else {
                let hdr = frame.hdr.as_ref().context("native planes missing")?;
                presenter.present(
                    &crate::hdr_present::P010Frame {
                        width: frame.width,
                        height: frame.height,
                        y: &hdr.y,
                        uv: &hdr.uv,
                        y_stride: hdr.y_stride,
                        uv_stride: hdr.uv_stride,
                    },
                    dest,
                    &overlays,
                    window,
                )?
            };
            self.ready = matches!(
                outcome,
                crate::hdr_present::PresentOutcome::Presented { .. }
            );
            self.dirty |= !self.ready;
        } else {
            self.canvas.present();
        }
        Ok(())
    }
}

fn mapping_shared(mappings: &[usize], position: usize) -> bool {
    mappings.get(position).is_some_and(|index| {
        mappings
            .iter()
            .enumerate()
            .any(|(i, value)| i != position && value == index)
    })
}
fn next_monitor(current: usize, count: usize) -> Option<usize> {
    (count > 1 && current < count).then(|| (current + 1) % count)
}

fn validate_frame(frame: &media::Image) -> Result<()> {
    protocol::validate_video_size(frame.width, frame.height)?;
    let check = |bytes: &[u8], stride: usize, row: usize, rows: usize| -> Result<()> {
        ensure!(
            rows > 0 && stride >= row && stride <= i32::MAX as usize,
            "invalid auxiliary image stride"
        );
        let required = stride
            .checked_mul(rows - 1)
            .and_then(|n| n.checked_add(row))
            .context("auxiliary image plane overflow")?;
        ensure!(required <= bytes.len(), "truncated auxiliary image plane");
        Ok(())
    };
    if let Some(nv) = &frame.nv12 {
        ensure!(
            frame.width.is_multiple_of(2) && frame.height.is_multiple_of(2),
            "odd auxiliary NV12 dimensions"
        );
        check(
            &nv.y,
            nv.y_stride,
            frame.width as usize,
            frame.height as usize,
        )?;
        check(
            &nv.uv,
            nv.uv_stride,
            frame.width as usize,
            frame.height as usize / 2,
        )?;
    } else {
        check(
            &frame.data,
            frame.stride,
            frame.width as usize * 3,
            frame.height as usize,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn monitor_remapping_wraps_and_shared_tracks_remain_live() {
        assert_eq!(next_monitor(0, 2), Some(1));
        assert_eq!(next_monitor(1, 2), Some(0));
        assert_eq!(next_monitor(0, 1), None);
        assert_eq!(next_monitor(0, 0), None);
        assert!(!mapping_shared(&[0, 1], 0));
        assert!(mapping_shared(&[1, 1], 0));
        assert!(mapping_shared(&[1, 1], 1));
        assert!(!mapping_shared(&[1], 0));
        assert!(!mapping_shared(&[], 0));
    }
    fn frame() -> media::Image {
        media::Image {
            decoded_at: Instant::now(),
            decoder_recovery: 0,
            video_group: Some(0),
            data: vec![0; 12],
            width: 2,
            height: 2,
            stride: 6,
            hdr: None,
            nv12: None,
            surface: None,
        }
    }
    #[test]
    fn auxiliary_upload_rejects_short_rgb_and_overflowing_strides() {
        let mut image = frame();
        assert!(validate_frame(&image).is_ok());
        image.data.truncate(11);
        assert!(validate_frame(&image).is_err());
        image.stride = usize::MAX;
        assert!(validate_frame(&image).is_err());
    }
    #[test]
    fn auxiliary_nv12_checks_both_planes_and_even_dimensions() {
        let mut image = frame();
        image.nv12 = Some(media::Nv12Image {
            y: vec![0; 4],
            uv: vec![0; 2],
            y_stride: 2,
            uv_stride: 2,
        });
        assert!(validate_frame(&image).is_ok());
        image.nv12.as_mut().unwrap().uv.truncate(1);
        assert!(validate_frame(&image).is_err());
        image.width = 3;
        assert!(validate_frame(&image).is_err());
    }
}
