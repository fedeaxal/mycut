use super::{FilmcraftApp, FrameKey, GpuState, frames, gpu_compositor_unsupported, plan_side};

impl FilmcraftApp {
    /// Enable the GPU compositor on the eframe wgpu device.
    pub fn set_wgpu(&mut self, rs: eframe::egui_wgpu::RenderState) {
        // Kept whatever the check below decides: the embedded Reaction page builds its engine
        // on this device even where the host's own compositor cannot — see the field's docs.
        self.wgpu_render_state = Some(rs.clone());
        if let Some(why) = gpu_compositor_unsupported(&rs.adapter) {
            log::warn!("GPU compositor disabled ({why}); compositing on the CPU");
            // Say so instead of letting the preview quietly crawl: "GPU 0 %, CPU 100 %" with no
            // explanation read like a renderer bug when the machine simply has no GPU access.
            self.ui.status = format!("{} ({why})", tl!("GPU acceleration is unavailable; preview renders on the CPU"));
            return;
        }
        // wgpu's default reaction to a validation error or lost device is to panic, which closed
        // the app (or left the monitors blank) on GPUs the compositor doesn't suit. Log instead and
        // fall back to the CPU compositor.
        //
        // One validation error is not fatal, though: "Failed to wait for GPU to come idle before
        // reconfiguring the Surface" is what a surface reconfigure says when the GPU is busy with
        // an encode or a composite — the old surface still works, and the next reconfigure
        // succeeds. Treating it as fatal put the whole application on the CPU path while
        // recording (the Reaction overlay windows' surfaces are reconfigured right as the encoder
        // starts). Same classification as the reference fix in egui-wgpu: catch, log, let the
        // next configure try again — only a long streak is fatal.
        let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = broken.clone();
        let busy_streak = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let streak = busy_streak.clone();
        // 连击的"连续"要有时钟来定:间隔超过一秒的失败是新一轮瞬时
        // 拥塞,不是上一场的延续——否则整场录制里偶发的几次失败也会
        // 累积成"30 连击",把合成器错误地打到 CPU 路径上。
        let last_busy = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let last_busy2 = last_busy.clone();
        rs.device.on_uncaptured_error(std::sync::Arc::new(move |e: eframe::wgpu::Error| {
            let text = e.to_string();
            // Two shapes of transient error share this path. "Failed to wait
            // for GPU to come idle" is a surface reconfigure that lost a race
            // with a busy GPU (an encode or a composite) — the old surface
            // still works and the next one succeeds. The scissor one is a
            // live window resize where egui rendered with the size it had
            // been told while the surface already had the next one — that
            // frame is dropped and the next is consistent again. Neither
            // says anything about the device being broken.
            let transient = text.contains("Failed to wait for GPU to come idle")
                || text.contains("is not contained in the render target");
            if transient {
                let now = std::time::Instant::now();
                let fresh = {
                    let mut last = last_busy2.lock().expect("busy-streak clock");
                    let fresh = last.is_none_or(|previous| now.duration_since(previous).as_secs_f32() > 1.0);
                    *last = Some(now);
                    fresh
                };
                let count = if fresh {
                    streak.store(1, std::sync::atomic::Ordering::Relaxed);
                    1
                } else {
                    streak.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
                };
                // First refusal of a streak is worth a line; the rest would be noise at 60 Hz.
                if count == 1 {
                    log::warn!("transient GPU error, retrying: {text}");
                } else {
                    log::debug!("transient GPU error again ({count} in a row)");
                }
                // A streak this long is not "transient" any more.
                if count > 30 {
                    log::error!("GPU error, switching to the CPU compositor: {text}");
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return;
            }
            log::error!("GPU error, switching to the CPU compositor: {text}");
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }));
        let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filmcraft_gpu::GpuCompositor::new(&rs.device, &rs.queue)));
        match made {
            Ok(compositor) if !broken.load(std::sync::atomic::Ordering::Relaxed) => {
                let max_texture = rs.device.limits().max_texture_dimension_2d;
                self.gpu = Some(GpuState { render_state: rs, compositor, texture: None, last_key: None, size: (0, 0), last_ms: 0.0, broken, max_texture });
            }
            _ => log::error!("GPU compositor failed to initialise; compositing on the CPU"),
        }
    }

    /// Drop the GPU compositor after a GPU error; the monitors composite on the CPU from then on.
    fn disable_gpu(&mut self, why: &str) {
        log::error!("GPU compositor disabled: {why}");
        if let Some(g) = self.gpu.take()
            && let Some(id) = g.texture
        {
            g.render_state.renderer.write().free_texture(&id);
        }
        self.ui.status = tl!("Graphics problem: switched to software compositing").into();
    }

    /// Composite a plan on the GPU and return the egui texture showing it.
    pub fn gpu_present(&mut self, key: FrameKey, plan: &frames::GpuPlan) -> Option<(egui::TextureId, (u32, u32))> {
        if self.gpu.as_ref()?.broken.load(std::sync::atomic::Ordering::Relaxed) {
            self.disable_gpu("device error");
            return None;
        }
        let g = self.gpu.as_mut()?;
        if g.last_key == Some(key)
            && let Some(t) = g.texture
        {
            return Some((t, g.size));
        }
        if plan_side(&plan.plan) > g.max_texture as usize {
            self.disable_gpu("frame larger than the GPU's maximum texture size");
            return None;
        }
        let t0 = web_time::Instant::now();
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (view, size) = g.compositor.composite_prepared(&plan.plan, Some(&plan.prepared));
            (view.clone(), size)
        }));
        let Ok((view, size)) = ran else {
            self.disable_gpu("compositor panicked");
            return None;
        };
        let g = self.gpu.as_mut()?;
        g.last_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let mut renderer = g.render_state.renderer.write();
        let id = match g.texture {
            Some(id) => {
                renderer.update_egui_texture_from_wgpu_texture(&g.render_state.device, &view, eframe::wgpu::FilterMode::Linear, id);
                id
            }
            None => renderer.register_native_texture(&g.render_state.device, &view, eframe::wgpu::FilterMode::Linear),
        };
        g.texture = Some(id);
        g.last_key = Some(key);
        g.size = size;
        Some((id, size))
    }
}
