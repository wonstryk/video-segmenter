// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// ───────────────────────────── helpers ─────────────────────────────

/// Run child processes without flashing a console window on Windows.
fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

fn fmt_time(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("VideoSegmenter")
}

/// Look for a tool next to our exe, then in our data dir, then on PATH.
fn tool(name: &str) -> PathBuf {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let p = dir.join(&file);
        if p.exists() {
            return p;
        }
    }
    let p = data_dir().join(&file);
    if p.exists() {
        return p;
    }
    PathBuf::from(file)
}

fn tool_works(name: &str) -> bool {
    quiet(Command::new(tool(name)).arg("-version"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn probe_duration(path: &Path) -> Option<f64> {
    let out = quiet(
        Command::new(tool("ffprobe")).args([
            "-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1",
        ]),
    )
    .arg(path)
    .output()
    .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn open_folder(path: &Path) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener).arg(path).spawn();
}

// ───────────────────────────── shared state ─────────────────────────────

#[derive(Clone, PartialEq)]
enum Phase {
    Idle,
    Running,
    Downloading,
    Done { count: usize, secs: f32, dir: PathBuf },
    Failed(String),
    Cancelled,
}

struct Shared {
    phase: Phase,
    progress: f32,
    status: String,
    speed: String,
    started: Option<Instant>,
}

type SharedState = Arc<Mutex<Shared>>;
type ChildSlot = Arc<Mutex<Option<Child>>>;

#[derive(Clone)]
struct Job {
    input: PathBuf,
    out_dir: PathBuf,
    prefix: String,
    ext: String,
    total: f64,
    seg_secs: f64,
    accurate: bool,
    no_audio: bool,
    crf: u8,
    preset: &'static str,
}

fn set(shared: &SharedState, f: impl FnOnce(&mut Shared)) {
    f(&mut shared.lock().unwrap());
}

// ───────────────────────────── worker: split ─────────────────────────────

fn run_split(job: Job, shared: SharedState, slot: ChildSlot, cancel: Arc<AtomicBool>, ctx: egui::Context) {
    let result = (|| -> Result<(usize, f32), String> {
        std::fs::create_dir_all(&job.out_dir).map_err(|e| format!("Cannot create output folder: {e}"))?;
        let pattern = job.out_dir.join(format!("{}_%03d.{}", job.prefix, job.ext));
        let est = (job.total / job.seg_secs).ceil() as u64;

        let mut cmd = Command::new(tool("ffmpeg"));
        quiet(&mut cmd).args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"]);
        cmd.arg(&job.input).args(["-map", "0:v:0"]);
        if !job.no_audio {
            cmd.args(["-map", "0:a?"]);
        }
        if job.accurate {
            cmd.args(["-c:v", "libx264", "-preset", job.preset, "-crf", &job.crf.to_string()]);
            cmd.args(["-force_key_frames", &format!("expr:gte(t,n_forced*{})", job.seg_secs)]);
            if !job.no_audio {
                cmd.args(["-c:a", "aac", "-b:a", "192k"]);
            }
        } else {
            cmd.args(["-c", "copy"]);
        }
        cmd.args([
            "-f", "segment", "-segment_time", &job.seg_secs.to_string(),
            "-reset_timestamps", "1", "-segment_format_options", "movflags=+faststart",
            "-progress", "pipe:1", "-stats_period", "0.2",
        ]);
        cmd.arg(&pattern).stdout(Stdio::piped()).stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| format!("Could not start FFmpeg: {e}"))?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        *slot.lock().unwrap() = Some(child);

        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = v.trim().parse::<i64>() {
                    let secs = (us.max(0) as f64 / 1e6).min(job.total);
                    set(&shared, |s| {
                        s.progress = (secs / job.total) as f32;
                        s.status = format!(
                            "{} / {}  •  segment {} of {}",
                            fmt_time(secs),
                            fmt_time(job.total),
                            ((secs / job.seg_secs) as u64 + 1).min(est),
                            est
                        );
                    });
                    ctx.request_repaint();
                }
            } else if let Some(v) = line.strip_prefix("speed=") {
                set(&shared, |s| s.speed = v.trim().to_string());
            }
        }

        let mut err_text = String::new();
        let _ = stderr.read_to_string(&mut err_text);
        let child = slot.lock().unwrap().take();
        let status = child.map(|mut c| c.wait()).transpose().map_err(|e| e.to_string())?;

        if cancel.load(Ordering::SeqCst) {
            return Err("__cancelled__".into());
        }
        if !status.map(|s| s.success()).unwrap_or(false) {
            let msg = err_text.trim();
            return Err(if msg.is_empty() { "FFmpeg failed.".into() } else { msg.chars().take(400).collect() });
        }
        let prefix = format!("{}_", job.prefix);
        let count = std::fs::read_dir(&job.out_dir)
            .map(|rd| rd.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).count())
            .unwrap_or(0);
        Ok((count, started.elapsed().as_secs_f32()))
    })();

    set(&shared, |s| {
        s.phase = match result {
            Ok((count, secs)) => {
                s.progress = 1.0;
                Phase::Done { count, secs, dir: job.out_dir.clone() }
            }
            Err(e) if e == "__cancelled__" => Phase::Cancelled,
            Err(e) => Phase::Failed(e),
        }
    });
    ctx.request_repaint();
}

// ───────────────────────────── worker: download FFmpeg ─────────────────────────────

const FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

fn run_download(shared: SharedState, ctx: egui::Context) {
    let result = (|| -> Result<(), String> {
        if !cfg!(windows) {
            return Err("Automatic download is Windows-only. Please install FFmpeg (e.g. `brew install ffmpeg` / `apt install ffmpeg`).".into());
        }
        let resp = ureq::get(FFMPEG_URL).call().map_err(|e| format!("Download failed: {e}"))?;
        let len: usize = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut reader = resp.into_reader();
        let mut data = Vec::with_capacity(len);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("Download failed: {e}"))?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if len > 0 {
                set(&shared, |s| {
                    s.progress = data.len() as f32 / len as f32;
                    s.status = format!("Downloading FFmpeg… {} / {} MB", data.len() >> 20, len >> 20);
                });
                ctx.request_repaint();
            }
        }
        set(&shared, |s| s.status = "Extracting…".into());
        ctx.request_repaint();

        let mut zip = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| format!("Bad archive: {e}"))?;
        let dir = data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
            let name = f.name().replace('\\', "/");
            for exe in ["ffmpeg.exe", "ffprobe.exe"] {
                if name.ends_with(&format!("bin/{exe}")) {
                    let mut out = std::fs::File::create(dir.join(exe)).map_err(|e| e.to_string())?;
                    std::io::copy(&mut f, &mut out).map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    })();
    set(&shared, |s| {
        s.progress = 0.0;
        s.status.clear();
        s.phase = match result {
            Ok(()) => Phase::Idle,
            Err(e) => Phase::Failed(e),
        };
    });
    ctx.request_repaint();
}

// ───────────────────────────── app ─────────────────────────────

const PRESETS: [&str; 4] = ["ultrafast", "veryfast", "fast", "medium"];

struct App {
    shared: SharedState,
    slot: ChildSlot,
    cancel: Arc<AtomicBool>,
    tools_ok: bool,

    input: Option<PathBuf>,
    total: f64,
    minutes: u32,
    seconds: u32,
    out_override: Option<PathBuf>,
    accurate: bool,
    no_audio: bool,
    crf: u8,
    preset: usize,
}

impl App {
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                phase: Phase::Idle,
                progress: 0.0,
                status: String::new(),
                speed: String::new(),
                started: None,
            })),
            slot: Arc::new(Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            tools_ok: tool_works("ffmpeg") && tool_works("ffprobe"),
            input: None,
            total: 0.0,
            minutes: 2,
            seconds: 0,
            out_override: None,
            accurate: false,
            no_audio: false,
            crf: 20,
            preset: 1,
        }
    }

    fn seg_secs(&self) -> f64 {
        (self.minutes * 60 + self.seconds).max(1) as f64
    }

    fn set_input(&mut self, p: PathBuf) {
        self.total = probe_duration(&p).unwrap_or(0.0);
        self.input = Some(p);
        self.out_override = None;
        set(&self.shared, |s| {
            s.phase = Phase::Idle;
            s.progress = 0.0;
        });
    }

    fn out_dir(&self) -> Option<PathBuf> {
        let input = self.input.as_ref()?;
        Some(self.out_override.clone().unwrap_or_else(|| {
            let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
            input.parent().unwrap_or(Path::new(".")).join(format!("{stem}_segments"))
        }))
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (Some(input), Some(out_dir)) = (self.input.clone(), self.out_dir()) else { return };
        let job = Job {
            prefix: input.file_stem().and_then(|s| s.to_str()).unwrap_or("segment").to_string(),
            ext: input.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_string(),
            input,
            out_dir,
            total: self.total,
            seg_secs: self.seg_secs(),
            accurate: self.accurate,
            no_audio: self.no_audio,
            crf: self.crf,
            preset: PRESETS[self.preset],
        };
        self.cancel.store(false, Ordering::SeqCst);
        set(&self.shared, |s| {
            s.phase = Phase::Running;
            s.progress = 0.0;
            s.status = "Starting…".into();
            s.speed.clear();
            s.started = Some(Instant::now());
        });
        let (sh, sl, ca, cx) = (self.shared.clone(), self.slot.clone(), self.cancel.clone(), ctx.clone());
        thread::spawn(move || run_split(job, sh, sl, ca, cx));
    }

    fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(c) = self.slot.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drag & drop a file anywhere on the window.
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone())) {
            if !matches!(self.shared.lock().unwrap().phase, Phase::Running) {
                self.set_input(p);
            }
        }

        let (phase, progress, status, speed, started) = {
            let s = self.shared.lock().unwrap();
            (s.phase.clone(), s.progress, s.status.clone(), s.speed.clone(), s.started)
        };
        let running = phase == Phase::Running;
        let downloading = phase == Phase::Downloading;
        if running || downloading {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Re-check tools once a download finished.
        if !self.tools_ok && phase == Phase::Idle {
            self.tools_ok = tool_works("ffmpeg") && tool_works("ffprobe");
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("✂  Video Segmenter");
            ui.label("Split long videos into equal-length clips — in seconds.");
            ui.add_space(10.0);

            // FFmpeg missing banner
            if !self.tools_ok {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label("FFmpeg is required and wasn't found.");
                    if downloading {
                        ui.add(egui::ProgressBar::new(progress).text(&status));
                    } else if ui.button("⬇  Download FFmpeg automatically (~80 MB)").clicked() {
                        set(&self.shared, |s| {
                            s.phase = Phase::Downloading;
                            s.progress = 0.0;
                            s.status = "Connecting…".into();
                        });
                        let (sh, cx) = (self.shared.clone(), ctx.clone());
                        thread::spawn(move || run_download(sh, cx));
                    }
                });
                ui.add_space(8.0);
            }

            ui.add_enabled_ui(!running && !downloading, |ui| {
                // File selection
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        if ui.button("📂  Choose video…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter("Video", &["mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m4v", "wmv", "mts", "m2ts"])
                                .pick_file()
                            {
                                self.set_input(p);
                            }
                        }
                        ui.weak("or drag & drop a file here");
                    });
                    if let Some(p) = &self.input {
                        ui.add_space(4.0);
                        ui.label(p.file_name().and_then(|n| n.to_str()).unwrap_or(""));
                        if self.total > 0.0 {
                            ui.weak(format!("Length: {}", fmt_time(self.total)));
                        } else {
                            ui.colored_label(egui::Color32::LIGHT_RED, "Could not read the video length.");
                        }
                    }
                });
                ui.add_space(8.0);

                // Duration
                ui.strong("Segment length");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.minutes).clamp_range(0..=600).suffix(" min"));
                    ui.add(egui::DragValue::new(&mut self.seconds).clamp_range(0..=59).suffix(" sec"));
                    ui.separator();
                    for (label, m, s) in [("30s", 0, 30), ("1m", 1, 0), ("2m", 2, 0), ("5m", 5, 0), ("10m", 10, 0)] {
                        if ui.small_button(label).clicked() {
                            self.minutes = m;
                            self.seconds = s;
                        }
                    }
                });
                if self.total > 0.0 {
                    ui.weak(format!("→ about {} segments", (self.total / self.seg_secs()).ceil() as u64));
                }
                ui.add_space(8.0);

                // Output
                ui.strong("Output folder");
                ui.horizontal(|ui| {
                    let text = self.out_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "—".into());
                    ui.add(egui::Label::new(text).truncate(true));
                    if self.input.is_some() && ui.button("Change…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.out_override = Some(p);
                        }
                    }
                });
                ui.add_space(8.0);

                // Options
                ui.checkbox(&mut self.accurate, "Frame-accurate cuts (re-encodes — slower)");
                if self.accurate {
                    ui.indent("adv", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Quality");
                            ui.add(egui::Slider::new(&mut self.crf, 14..=30).text("CRF (lower = better)"));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Speed");
                            egui::ComboBox::from_id_source("preset")
                                .selected_text(PRESETS[self.preset])
                                .show_ui(ui, |ui| {
                                    for (i, p) in PRESETS.iter().enumerate() {
                                        ui.selectable_value(&mut self.preset, i, *p);
                                    }
                                });
                        });
                    });
                } else {
                    ui.weak("Fast mode copies the video as-is. Cuts snap to the nearest keyframe (±1–2 s).");
                }
                ui.checkbox(&mut self.no_audio, "Remove audio");
            });

            ui.add_space(14.0);

            // Action button
            let can_start = self.tools_ok && self.input.is_some() && self.total > 0.0 && !downloading;
            if running {
                if ui.add_sized([ui.available_width(), 36.0], egui::Button::new("⏹  Cancel")).clicked() {
                    self.stop();
                }
            } else if ui
                .add_enabled(can_start, egui::Button::new(egui::RichText::new("▶  Split video").size(16.0)).min_size(egui::vec2(ui.available_width(), 38.0)))
                .clicked()
            {
                self.start(ctx);
            }

            // Progress
            if running {
                ui.add_space(10.0);
                let eta = started
                    .filter(|_| progress > 0.01)
                    .map(|t| {
                        let el = t.elapsed().as_secs_f64();
                        format!("  •  ETA {}", fmt_time(el / progress as f64 - el))
                    })
                    .unwrap_or_default();
                ui.add(egui::ProgressBar::new(progress).desired_height(22.0).show_percentage().animate(true));
                ui.weak(format!("{status}{}{eta}", if speed.is_empty() { String::new() } else { format!("  •  {speed}") }));
            }

            // Result
            ui.add_space(10.0);
            match &phase {
                Phase::Done { count, secs, dir } => {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, format!("✔ Created {count} segments in {secs:.1}s"));
                    if ui.button("📁  Open output folder").clicked() {
                        open_folder(dir);
                    }
                }
                Phase::Failed(e) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("✖ {e}"));
                }
                Phase::Cancelled => {
                    ui.colored_label(egui::Color32::YELLOW, "Cancelled.");
                }
                _ => {}
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 600.0])
            .with_min_inner_size([480.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("Video Segmenter", options, Box::new(|_cc| Box::new(App::new())))
}
