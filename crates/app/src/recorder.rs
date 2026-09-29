//! Screen-capture replay recorder. Captures the app window region at a fixed
//! rate on a worker thread; `stop()` joins it and encodes the frames as an
//! animated GIF — a self-contained replay of what the session did.

use std::io::BufWriter;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FPS: u64 = 8;
const MAX_SECS: u64 = 120;
/// scale frames down so GIFs stay a sane size
const SCALE: f32 = 0.5;

struct CaptureRegion {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    /// latest captured region — updated per frame if the window moves
    region: Arc<Mutex<CaptureRegion>>,
    handle: Option<JoinHandle<Vec<image::RgbaImage>>>,
    started: Instant,
}

impl Recorder {
    pub fn start(x: f64, y: f64, w: f64, h: f64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let region = Arc::new(Mutex::new(CaptureRegion {
            x: x as u32,
            y: y as u32,
            w: w.max(1.0) as u32,
            h: h.max(1.0) as u32,
        }));
        let (stop_t, region_t) = (stop.clone(), region.clone());
        let handle = std::thread::spawn(move || {
            let mut frames: Vec<image::RgbaImage> = Vec::new();
            let screens = screenshots::Screen::all().unwrap_or_default();
            let Some(screen) = screens.into_iter().next() else {
                return frames;
            };
            let frame_dur = Duration::from_millis(1000 / FPS);
            let max_frames = (FPS * MAX_SECS) as usize;
            while !stop_t.load(Ordering::Relaxed) && frames.len() < max_frames {
                let t0 = Instant::now();
                let (x, y, w, h) = {
                    let r = region_t.lock().unwrap();
                    (r.x, r.y, r.w, r.h)
                };
                if let Ok(img) = screen.capture_area(x as i32, y as i32, w, h) {
                    let (dw, dh) = (
                        (w as f32 * SCALE).max(1.0) as u32,
                        (h as f32 * SCALE).max(1.0) as u32,
                    );
                    let small = image::imageops::resize(
                        &img,
                        dw,
                        dh,
                        image::imageops::FilterType::Nearest,
                    );
                    frames.push(small);
                }
                if let Some(rem) = frame_dur.checked_sub(t0.elapsed()) {
                    std::thread::sleep(rem);
                }
            }
            frames
        });
        Self {
            stop,
            region,
            handle: Some(handle),
            started: Instant::now(),
        }
    }

    /// Keep the capture rectangle tracking the window (called per frame).
    pub fn update_region(&self, x: f64, y: f64, w: f64, h: f64) {
        let mut r = self.region.lock().unwrap();
        *r = CaptureRegion {
            x: x as u32,
            y: y as u32,
            w: w.max(1.0) as u32,
            h: h.max(1.0) as u32,
        };
    }

    /// Join the capture thread and write the frames as a GIF. Returns the
    /// output path on success.
    pub fn stop(&mut self, dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
        self.stop.store(true, Ordering::Relaxed);
        let Some(h) = self.handle.take() else {
            return Err("recorder not running".into());
        };
        let frames = h.join().map_err(|_| "capture thread panicked".to_string())?;
        if frames.is_empty() {
            return Err("no frames captured".into());
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let name = format!(
            "replay-{}.gif",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let path = dir.join(name);
        let (w, hgt) = (frames[0].width(), frames[0].height());
        let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        let mut enc = gif::Encoder::new(BufWriter::new(file), w as u16, hgt as u16, &[])
            .map_err(|e| e.to_string())?;
        enc.set_repeat(gif::Repeat::Infinite).map_err(|e| e.to_string())?;
        let delay_cs = (100 / FPS) as u16;
        for img in &frames {
            let mut buf = img.clone().into_raw();
            let mut frame = gif::Frame::from_rgba_speed(w as u16, hgt as u16, &mut buf, 20);
            frame.delay = delay_cs;
            enc.write_frame(&frame).map_err(|e| e.to_string())?;
        }
        Ok(path)
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
