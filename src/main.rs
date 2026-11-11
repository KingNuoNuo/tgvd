#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! TGVD · 电报视频下载器
//!
//! 下载管理器式界面：左侧导航，中间任务表（名称 / 大小 / 进度 / 状态 / 速度 /
//! 连接数），下方工具栏与选中任务详情。所有流量都走 MTProto 账号通道。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};
use tgvd::telegram::{self, Cmd as TgCmd, Ev as TgEv, SearchHit, StartReq, TaskInfo};
use tokio::sync::mpsc::UnboundedSender;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("TGVD · 电报视频下载器"),
        ..Default::default()
    };
    eframe::run_native(
        "tgvd",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

// ---------------------------------------------------------------- theme

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    card: Color32,
    line: Color32,
    text: Color32,
    weak: Color32,
    accent: Color32,
    ok: Color32,
    warn: Color32,
    err: Color32,
    sel: Color32,
    hover: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: Color32::from_rgb(23, 24, 28),
            panel: Color32::from_rgb(29, 30, 35),
            card: Color32::from_rgb(35, 37, 42),
            line: Color32::from_rgb(50, 53, 61),
            text: Color32::from_rgb(228, 230, 235),
            weak: Color32::from_rgb(150, 156, 166),
            accent: Color32::from_rgb(90, 140, 255),
            ok: Color32::from_rgb(74, 200, 130),
            warn: Color32::from_rgb(240, 176, 70),
            err: Color32::from_rgb(248, 113, 113),
            sel: Color32::from_rgb(45, 55, 82),
            hover: Color32::from_rgb(38, 40, 46),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(255, 255, 255),
            panel: Color32::from_rgb(246, 247, 249),
            card: Color32::WHITE,
            line: Color32::from_rgb(229, 231, 235),
            text: Color32::from_rgb(31, 35, 41),
            weak: Color32::from_rgb(138, 144, 153),
            accent: Color32::from_rgb(47, 107, 255),
            ok: Color32::from_rgb(22, 163, 74),
            warn: Color32::from_rgb(180, 83, 9),
            err: Color32::from_rgb(220, 38, 38),
            sel: Color32::from_rgb(234, 240, 255),
            hover: Color32::from_rgb(245, 247, 250),
        }
    }
}

fn cr(n: u8) -> egui::CornerRadius {
    egui::CornerRadius::same(n)
}

fn apply_theme(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut style = (*ctx.style()).clone();
    style.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let v = &mut style.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.extreme_bg_color = p.panel;
    v.window_corner_radius = cr(10);
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0_f32, p.accent);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, p.text);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, p.line);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.fg_stroke = egui::Stroke::new(1.0_f32, p.text);
        w.bg_stroke = egui::Stroke::new(1.0_f32, p.line);
        w.corner_radius = cr(7);
    }
    v.widgets.inactive.weak_bg_fill = p.panel;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.active.weak_bg_fill = p.sel;
    v.widgets.open.weak_bg_fill = p.panel;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    ctx.set_style(style);
}

fn setup_fonts(ctx: &egui::Context) {
    // egui's built-in fonts have no CJK glyphs. Borrow one from Windows instead,
    // so the exe stays single-file (no font asset to ship).
    let candidates = [
        "C:\\Windows\\Fonts\\Deng.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];
    let bytes = candidates.iter().find_map(|p| std::fs::read(p).ok());
    let Some(bytes) = bytes else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "cjk".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("cjk".to_owned());
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------- data

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Search,
    Downloads,
    Logs,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum TgNeed {
    None,
    Code,
    Password,
}

#[derive(Clone)]
struct Task {
    id: u64,
    name: String,
    size: u64,
    done: u64,
    status: String,
    speed: f64,
    conns: u32,
    link: String,
    path: String,
    /// Parallel streams this task was queued with. Saved with the task list so a
    /// restored task rebuilds the same segment plan and can resume its `.tgvd`.
    segments: usize,
}

impl Task {
    fn progress(&self) -> f32 {
        if self.size == 0 {
            if self.is_done() {
                1.0
            } else {
                0.0
            }
        } else {
            (self.done as f32 / self.size as f32).clamp(0.0, 1.0)
        }
    }

    fn is_done(&self) -> bool {
        self.status == "完成" || self.status == "已存在"
    }

    fn is_fail(&self) -> bool {
        self.status.starts_with("错误")
            || self.status.contains("未完成")
            || self.status.contains("失败")
    }

    fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "下载中" | "排队中")
    }

    /// Stopped, failed or otherwise parked: it is safe to (re)start from here.
    /// An active task must never be started twice — two writers would race on
    /// the same `.part` file.
    fn is_resumable(&self) -> bool {
        !self.is_active() && !self.is_done()
    }

    /// Seconds left at the current speed; `None` when it cannot be estimated.
    fn eta(&self) -> Option<u64> {
        if !self.is_active() || self.size == 0 || self.speed <= 1.0 {
            return None;
        }
        let left = self.size.saturating_sub(self.done);
        if left == 0 {
            return None;
        }
        Some(((left as f64 / self.speed).ceil() as u64).max(1))
    }
}

/// `5400` -> `1时30分`. Deliberately coarse: the estimate is derived from a
/// smoothed speed, and showing seconds would only make it flicker.
fn format_eta(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}秒")
    } else if secs < 3600 {
        format!("{}分{}秒", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}时{}分", secs / 3600, secs % 3600 / 60)
    } else {
        format!("{}天{}时", secs / 86_400, secs % 86_400 / 3600)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    out_dir: String,
    files: u32,
    segments: u32,
    limit: u32,
    overwrite: bool,
    dark: bool,
}

fn default_out_dir() -> String {
    let base = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join("Downloads")
        .join("TGVD")
        .to_string_lossy()
        .to_string()
}

impl Settings {
    fn load() -> Self {
        let fallback = Self {
            out_dir: default_out_dir(),
            files: 3,
            segments: 8,
            limit: 20,
            overwrite: false,
            dark: false,
        };
        let path = settings_path();
        let Ok(bytes) = std::fs::read(&path) else {
            return fallback;
        };
        serde_json::from_slice::<Settings>(&bytes).unwrap_or(fallback)
    }

    fn save(&self) {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(&path, text);
        }
    }
}

fn settings_path() -> PathBuf {
    telegram::session_path().with_file_name("settings.json")
}

// ---------------------------------------------------------------- task list
//
// The queue used to live in memory only, so closing the window threw the whole
// download list away. It is mirrored to `tasks.json` beside the settings, and a
// restored row can be started again from its link (see `Cmd::Start`).

fn tasks_path() -> PathBuf {
    telegram::session_path().with_file_name("tasks.json")
}

fn default_segments() -> usize {
    8
}

/// One row of `tasks.json`. Flat and forgiving on purpose: unknown fields are
/// ignored and missing ones fall back to defaults, so a list written by an older
/// build still loads instead of being silently dropped.
#[derive(serde::Serialize, serde::Deserialize)]
struct TaskRec {
    id: u64,
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    done: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    path: String,
    #[serde(default = "default_segments")]
    segments: usize,
}

fn load_tasks() -> Vec<Task> {
    let Ok(bytes) = std::fs::read(tasks_path()) else {
        return Vec::new();
    };
    let recs: Vec<TaskRec> = serde_json::from_slice(&bytes).unwrap_or_default();
    let mut out: Vec<Task> = recs
        .into_iter()
        .filter(|r| !r.name.is_empty() && !r.path.is_empty())
        .map(|r| {
            let parked = r.status.is_empty() || r.status == "下载中" || r.status == "排队中";
            let mut t = Task {
                id: r.id,
                name: r.name,
                size: r.size,
                done: r.done,
                // Nothing survives a restart as "running": a task that was
                // mid-transfer is shown as stopped so it can be restarted.
                status: if parked { "已停止".into() } else { r.status },
                speed: 0.0,
                conns: 0,
                link: r.link,
                path: r.path,
                segments: r.segments.max(1),
            };
            reconcile(&mut t);
            t
        })
        .collect();
    out.sort_by_key(|t| t.id);
    out
}

/// Brings a restored row in line with what is actually on disk.
///
/// The JSON is written at most once a second and not at all when the app is
/// killed, so the file may lag behind: a finished download should still show as
/// finished, and a half-written one should show the bytes it really has.
fn reconcile(t: &mut Task) {
    let dest = std::path::Path::new(&t.path);
    if let Ok(meta) = std::fs::metadata(dest) {
        if meta.is_file() {
            let len = meta.len();
            if t.size == 0 || len >= t.size {
                t.size = len.max(t.size);
                t.done = t.size;
                t.status = "完成".into();
                t.speed = 0.0;
                t.conns = 0;
                return;
            }
        }
    }
    if t.is_done() {
        return;
    }
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let state = dest.with_file_name(format!("{name}.part.tgvd"));
    let Ok(bytes) = std::fs::read(&state) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    if let Some(segs) = v.get("segs").and_then(|s| s.as_array()) {
        let sum: u64 = segs.iter().filter_map(|x| x.as_u64()).sum();
        if sum > 0 {
            t.done = sum;
        }
        // The segment count has to match or the resume is thrown away, so take
        // it from the file rather than from the (possibly older) record.
        if !segs.is_empty() {
            t.segments = segs.len();
        }
    }
    if let Some(size) = v.get("size").and_then(|x| x.as_u64()) {
        if size > 0 {
            t.size = size;
        }
    }
}

fn save_tasks(tasks: &[Task]) {
    let recs: Vec<TaskRec> = tasks
        .iter()
        .map(|t| TaskRec {
            id: t.id,
            name: t.name.clone(),
            size: t.size,
            // An in-flight task's `done` is a snapshot; `reconcile` re-reads the
            // real number from the `.tgvd` file on the next start anyway.
            done: t.done,
            status: t.status.clone(),
            link: t.link.clone(),
            path: t.path.clone(),
            segments: t.segments,
        })
        .collect();
    let Ok(text) = serde_json::to_vec_pretty(&recs) else {
        return;
    };
    let path = tasks_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write beside the target and rename: being killed mid-write would otherwise
    // leave a truncated file that loads as an empty list, which reads exactly
    // like "the history disappeared again".
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ---------------------------------------------------------------- app

struct App {
    page: Page,
    dark: bool,
    settings: Settings,

    tasks: Vec<Task>,
    selected: HashSet<u64>,
    current: Option<u64>,
    logs: Vec<String>,
    resolving: bool,

    // 添加任务对话框
    add_open: bool,
    add_links: String,

    // 搜索页
    search_input: String,
    search_kw: String,
    search_note: String,
    searching: bool,
    hits: Vec<SearchHit>,
    hit_sel: HashSet<usize>,

    // MTProto
    tg_tx: UnboundedSender<TgCmd>,
    tg_ev: mpsc::Receiver<TgEv>,
    tg_api_id: String,
    tg_api_hash: String,
    tg_phone: String,
    tg_code: String,
    tg_pw: String,
    tg_stage: String,
    tg_user: String,
    tg_need: TgNeed,
    tg_started: Option<Instant>,
    tg_pending_until: Option<Instant>,
    session_file: String,
    /// `TGVD_PROBE=@channel` runs one search right after login, so the naming
    /// and the result list can be checked from the log without a mouse.
    probe: Option<String>,
    /// `TGVD_PROBE_DL=1` (with `TGVD_PROBE`) also downloads the first hit, so the
    /// pick-then-download path can be checked end to end without a mouse.
    probe_dl: bool,

    settings_dirty_at: Option<Instant>,
    /// Set when the task list changed and needs writing to `tasks.json`.
    tasks_dirty_at: Option<Instant>,
    /// A change worth writing immediately (a task appeared, finished or was
    /// removed) rather than in the next throttled tick. Progress-only updates
    /// stay throttled.
    tasks_urgent: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let settings = Settings::load();
        apply_theme(&cc.egui_ctx, settings.dark);

        let (tg_ev_tx, tg_ev) = mpsc::channel();
        // Kept in the user profile, not next to the exe, so a new build keeps the login.
        let session_file = telegram::prepare_session();
        let tg_tx = telegram::spawn(tg_ev_tx, session_file.clone()).tx;

        let mut app = Self {
            page: Page::Downloads,
            dark: settings.dark,
            settings,
            tasks: load_tasks(),
            selected: HashSet::new(),
            current: None,
            logs: Vec::new(),
            resolving: false,
            add_open: false,
            add_links: String::new(),
            search_input: String::new(),
            search_kw: String::new(),
            search_note: String::new(),
            searching: false,
            hits: Vec::new(),
            hit_sel: HashSet::new(),
            tg_tx,
            tg_ev,
            tg_api_id: telegram::DEFAULT_API_ID.to_string(),
            tg_api_hash: telegram::DEFAULT_API_HASH.to_string(),
            tg_phone: String::new(),
            tg_code: String::new(),
            tg_pw: String::new(),
            tg_stage: "未登录".into(),
            tg_user: String::new(),
            tg_need: TgNeed::None,
            tg_started: None,
            tg_pending_until: None,
            session_file: session_file.to_string_lossy().to_string(),
            probe_dl: std::env::var_os("TGVD_PROBE_DL").is_some(),
            probe: std::env::var("TGVD_PROBE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            settings_dirty_at: None,
            tasks_dirty_at: None,
            tasks_urgent: false,
        };
        app.push_log(format!("输出目录：{}", app.settings.out_dir));
        if !app.tasks.is_empty() {
            let left = app.tasks.iter().filter(|t| !t.is_done()).count();
            app.push_log(format!(
                "已恢复上次的下载列表：{} 个任务{}",
                app.tasks.len(),
                if left > 0 {
                    format!("，其中 {left} 个没下完，选中后点「开始」可以接着下")
                } else {
                    String::new()
                }
            ));
            app.current = app.tasks.first().map(|t| t.id);
        }

        // Debug aid: see `seed_demo`. Values 2/3 also open the dialog / settings
        // page, so the layout can be checked without a mouse.
        match std::env::var("TGVD_DEMO").ok().as_deref() {
            Some("1") | Some("") => app.seed_demo(),
            Some("2") => {
                app.seed_demo();
                app.add_open = true;
                app.add_links =
                    "https://t.me/xgduanju/2942
https://t.me/s/telegram/441".into();
            }
            Some("3") => {
                app.seed_demo();
                app.page = Page::Settings;
            }
            Some("4") | Some("5") => {
                app.page = Page::Search;
                app.search_input = "https://t.me/xgduanju".into();
                app.search_kw = "EP".into();
                app.seed_hits();
                if std::env::var("TGVD_DEMO").ok().as_deref() == Some("5") {
                    app.hit_sel.insert(0);
                    app.hit_sel.insert(2);
                }
            }
            _ => {}
        }

        // Reuse the saved session on startup: no SMS code when the file is there.
        if session_file.exists() {
            app.tg_stage = "正在恢复上次的登录…".into();
            app.tg_send(TgCmd::Login {
                api_id: telegram::DEFAULT_API_ID,
                api_hash: telegram::DEFAULT_API_HASH.to_string(),
                phone: String::new(),
            });
        }
        app
    }

    fn logged_in(&self) -> bool {
        !self.tg_user.is_empty()
    }

    /// Debug aid: `set TGVD_DEMO=1` fills the table with fake rows so the layout
    /// can be checked without logging in to Telegram.
    fn seed_demo(&mut self) {
        let demo = [
            ("EP01 都市异能 1080p.mp4", 780_000_000u64, 356_000_000u64, "下载中", 4u32),
            ("EP02 都市异能 1080p.mp4", 812_000_000, 812_000_000, "完成", 0),
            ("EP03 都市异能 1080p.mp4", 655_000_000, 120_000_000, "已停止", 0),
            ("合集·第一季 全 24 集.mkv", 2_400_000_000, 980_000_000, "下载中", 8),
            ("[METADATA] 采样占位.mp4", 0, 0, "错误: 该频道禁止转发", 0),
        ];
        for (i, (name, size, done, status, conns)) in demo.iter().enumerate() {
            self.tasks.push(Task {
                id: (i + 1) as u64,
                name: name.to_string(),
                size: *size,
                done: *done,
                status: status.to_string(),
                speed: if *conns > 0 { 3_400_000.0 } else { 0.0 },
                conns: *conns,
                link: format!("https://t.me/xgduanju/{}", 2940 + i),
                path: std::path::Path::new(&self.settings.out_dir)
                    .join(name.to_string())
                    .to_string_lossy()
                    .to_string(),
                segments: 8,
            });
        }
        self.selected.insert(1);
        self.current = Some(1);
        self.push_log("演示模式（TGVD_DEMO=1）：上表为假数据，仅用于检查排版".into());
    }

    /// Debug aid: `TGVD_DEMO=4` fills the search page with fake results.
    fn seed_hits(&mut self) {
        let demo = [
            ("EP01 都市异能觉醒.mp4", 780_000_000u64, "2026-09-21 20:15", "9月21日 EP01 都市异能觉醒 #短剧"),
            ("EP02 都市异能觉醒.mp4", 812_000_000, "2026-09-22 20:10", "9月22日 EP02 都市异能觉醒 #短剧"),
            ("EP03 都市异能觉醒.mp4", 655_000_000, "2026-09-23 20:31", "9月23日 EP03 都市异能觉醒 #短剧"),
            ("EP04 都市异能觉醒.mp4", 903_000_000, "2026-09-24 19:58", "9月24日 EP04 都市异能觉醒 #短剧"),
            ("合集·第一季 全 24 集.mkv", 2_400_000_000, "2026-10-01 12:00", "第一季合集 全24集 1080p"),
            ("xgduanju_2950.mp4", 0, "2026-10-02 09:12", "（这条没有配文，也没有文件名）"),
        ];
        for (i, (name, size, date, caption)) in demo.iter().enumerate() {
            self.hits.push(SearchHit {
                idx: i,
                name: name.to_string(),
                size: *size,
                date: date.to_string(),
                caption: caption.to_string(),
                link: format!("https://t.me/xgduanju/{}", 2945 + i),
            });
        }
        self.search_note = format!("找到 {} 个视频，勾选后点「下载选中」", self.hits.len());
        self.push_log("演示模式（TGVD_DEMO=4）：搜索结果为假数据".into());
    }

    // --- worker plumbing ------------------------------------------------

    /// Sends a command and marks it as in flight, so the window keeps repainting
    /// while we wait on the network (otherwise the stage text looks frozen).
    fn tg_send(&mut self, cmd: TgCmd) {
        self.tg_started = Some(Instant::now());
        self.tg_pending_until = Some(Instant::now() + Duration::from_secs(30));
        let _ = self.tg_tx.send(cmd);
    }

    /// Sends a command without arming the "正在…" indicator — used by settings
    /// that apply immediately, where there is nothing to wait for. Dragging the
    /// concurrency slider would otherwise leave the UI spinning for 30s.
    fn tg_send_quiet(&mut self, cmd: TgCmd) {
        let _ = self.tg_tx.send(cmd);
    }

    fn tg_pending(&self) -> bool {
        self.tg_pending_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    fn tg_done(&mut self) {
        self.tg_pending_until = None;
        self.tg_started = None;
    }

    fn push_log(&mut self, line: String) {
        self.logs.push(line);
        if self.logs.len() > 500 {
            self.logs.drain(..200);
        }
    }

    fn touch_settings(&mut self) {
        self.settings_dirty_at = Some(Instant::now());
    }

    fn flush_settings(&mut self) {
        if let Some(t) = self.settings_dirty_at {
            if t.elapsed() >= Duration::from_millis(600) {
                self.settings.save();
                self.settings_dirty_at = None;
            }
        }
    }

    fn touch_tasks(&mut self) {
        if self.tasks_dirty_at.is_none() {
            self.tasks_dirty_at = Some(Instant::now());
        }
    }

    /// A state change that must hit the disk on this very frame.
    fn touch_tasks_now(&mut self) {
        self.tasks_urgent = true;
    }

    /// Mirrors the queue to disk.
    ///
    /// Progress updates are throttled to one write a second (there is one every
    /// 300 ms per task), but anything that changes a *status* is written at once:
    /// the window hook cannot be relied on to fire, and losing "this task
    /// finished / was removed" for a second is what makes a list look stale.
    fn flush_tasks(&mut self) {
        if self.tasks_urgent {
            save_tasks(&self.tasks);
            self.tasks_urgent = false;
            self.tasks_dirty_at = None;
            return;
        }
        if let Some(t) = self.tasks_dirty_at {
            if t.elapsed() >= Duration::from_millis(1000) {
                save_tasks(&self.tasks);
                self.tasks_dirty_at = None;
            }
        }
    }

    fn task(&self, id: u64) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Ids already on screen — restored rows included — have to stay unique, so
    /// new tasks must start above the highest one.
    fn next_task_id(&self) -> u64 {
        self.tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1
    }

    fn selected_ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .tasks
            .iter()
            .filter(|t| self.selected.contains(&t.id))
            .map(|t| t.id)
            .collect();
        v.sort_unstable();
        v
    }

    fn pump(&mut self) -> bool {
        let mut any = false;
        while let Ok(ev) = self.tg_ev.try_recv() {
            any = true;
            match ev {
                TgEv::Log(s) => {
                    if s.contains("失败") || s.contains("超时") {
                        self.tg_done();
                    }
                    self.push_log(s);
                }
                TgEv::NeedCode => {
                    self.tg_need = TgNeed::Code;
                    self.tg_stage = "等待验证码".into();
                    self.tg_done();
                }
                TgEv::NeedPassword(hint) => {
                    self.tg_done();
                    self.tg_need = TgNeed::Password;
                    self.tg_stage = if hint.trim().is_empty() {
                        "需要两步验证密码".into()
                    } else {
                        format!("需要两步验证密码（提示：{hint}）")
                    };
                }
                TgEv::Authorized(who) => {
                    self.tg_done();
                    self.tg_need = TgNeed::None;
                    self.tg_user = who.clone();
                    self.tg_stage = format!("已登录：{who}");
                    self.push_log(format!("账号登录成功：{who}"));
                    if let Some(target) = self.probe.take() {
                        self.search_input = target;
                        self.page = Page::Search;
                        self.start_search();
                    }
                }
                TgEv::LoggedOut => {
                    self.tg_done();
                    self.tg_need = TgNeed::None;
                    self.tg_user.clear();
                    self.tg_stage = "已退出登录（凭证已删除）".into();
                }
                TgEv::Stage(s) => {
                    if s.starts_with("未登录")
                        || s.starts_with("检查登录超时")
                        || s.starts_with("检查登录失败")
                        || s.starts_with("登录失败")
                        || s.starts_with("需要先注册")
                    {
                        self.tg_done();
                    }
                    self.tg_stage = s;
                }
                TgEv::Added(info) => {
                    let id = info.id;
                    // A task restored from `tasks.json` already has a row; the
                    // engine's `Added` only fills in the live details, so update
                    // it instead of appending a duplicate line.
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.size = info.size;
                        t.link = info.link;
                        t.path = info.path;
                        t.segments = info.segments;
                        if !t.is_done() {
                            t.status = "排队中".into();
                        }
                    } else {
                        self.tasks.push(Task::from(info));
                    }
                    self.tg_done();
                    if self.current.is_none() {
                        self.current = Some(id);
                    }
                    self.touch_tasks_now();
                }
                TgEv::Progress {
                    id,
                    done,
                    speed,
                    conns,
                } => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.done = done;
                        // The engine measures speed over a 300 ms window, which
                        // makes both the speed column and the ETA jitter: keep a
                        // running average so the numbers stay readable.
                        t.speed = if t.speed <= 0.0 {
                            speed
                        } else {
                            t.speed * 0.65 + speed * 0.35
                        };
                        t.conns = conns;
                        if t.status == "排队中" || t.status == "已停止" {
                            t.status = "下载中".into();
                        }
                    }
                }
                TgEv::Status { id, status } => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.status = status;
                    }
                    self.touch_tasks_now();
                }
                TgEv::Done { id, status } => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        if (status == "完成" || status == "已存在") && t.size > 0 {
                            t.done = t.size;
                        }
                        t.speed = 0.0;
                        t.conns = 0;
                        t.status = status;
                    }
                    self.touch_tasks_now();
                }
                TgEv::ResolveDone => {
                    self.resolving = false;
                }
                TgEv::SearchStart => {
                    self.searching = true;
                    self.hits.clear();
                    self.hit_sel.clear();
                    self.search_note = "正在搜索…".into();
                }
                TgEv::SearchHit(h) => {
                    self.hits.push(h);
                }
                TgEv::SearchDone { total } => {
                    self.searching = false;
                    if total > 0 && self.probe_dl {
                        self.probe_dl = false;
                        self.hit_sel = self.hits.iter().take(1).map(|h| h.idx).collect();
                        self.push_log("PROBE：自动下载第 1 个结果".into());
                        self.download_picked();
                    }
                    self.search_note = if total == 0 {
                        "没有找到视频".to_string()
                    } else {
                        format!("找到 {total} 个视频，勾选后点「下载选中」")
                    };
                    self.tg_done();
                }
                TgEv::SearchFail(e) => {
                    self.searching = false;
                    self.search_note = e;
                    self.tg_done();
                }
            }
        }
        if any {
            // Any event may have touched a row; the write itself is throttled.
            self.touch_tasks();
        }
        any
    }

    // --- actions --------------------------------------------------------

    fn submit_links(&mut self) {
        let links: Vec<String> = self
            .add_links
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
            .collect();
        if links.is_empty() {
            self.push_log("没有可添加的链接".into());
            return;
        }
        if !self.logged_in() {
            self.push_log("请先在「设置 → 账号」里登录 Telegram 账号".into());
            self.page = Page::Settings;
            return;
        }
        self.resolving = true;
        self.push_log(format!(
            "添加 {} 个链接（同时 {} 个文件，每个 {} 连接）",
            links.len(),
            self.settings.files,
            self.settings.segments
        ));
        let min_id = self.next_task_id();
        self.tg_send(TgCmd::Add {
            links,
            limit: self.settings.limit.max(1) as usize,
            out_dir: PathBuf::from(self.settings.out_dir.trim()),
            overwrite: self.settings.overwrite,
            files: self.settings.files as usize,
            segments: self.settings.segments as usize,
            // Keyword searching has its own page now; a pasted link means
            // "queue whatever this channel has".
            keyword: String::new(),
            min_id,
        });
        self.add_links.clear();
        self.add_open = false;
        self.page = Page::Downloads;
    }

    // --- search ---------------------------------------------------------

    fn start_search(&mut self) {
        let input = self.search_input.trim().to_string();
        if input.is_empty() {
            self.push_log("先在搜索页填频道（@用户名 或 t.me 链接）".into());
            return;
        }
        if !self.logged_in() {
            self.push_log("请先在「设置」里登录 Telegram 账号".into());
            self.page = Page::Settings;
            return;
        }
        self.searching = true;
        self.hits.clear();
        self.hit_sel.clear();
        self.search_note = "正在搜索…".into();
        self.tg_send(TgCmd::Search {
            input,
            keyword: self.search_kw.trim().to_string(),
            limit: self.settings.limit.max(1) as usize,
        });
    }

    fn download_picked(&mut self) {
        let mut picks: Vec<usize> = self.hit_sel.iter().copied().collect();
        picks.sort_unstable();
        if picks.is_empty() {
            self.push_log("先勾选要下载的视频".into());
            return;
        }
        self.push_log(format!(
            "下载选中的 {} 个视频（同时 {} 个文件，每个 {} 连接）",
            picks.len(),
            self.settings.files,
            self.settings.segments
        ));
        let min_id = self.next_task_id();
        self.tg_send(TgCmd::DownloadPicked {
            picks,
            out_dir: PathBuf::from(self.settings.out_dir.trim()),
            overwrite: self.settings.overwrite,
            files: self.settings.files as usize,
            segments: self.settings.segments as usize,
            min_id,
        });
        self.hit_sel.clear();
        self.page = Page::Downloads;
    }

    /// Starts (or resumes) the selected tasks. Anything already transferring is
    /// skipped, and so is anything already finished — re-queueing those would
    /// put a second writer on the same `.part` file.
    fn start(&mut self, ids: Vec<u64>) {
        let runnable: Vec<StartReq> = ids
            .iter()
            .filter_map(|id| self.task(*id))
            .filter(|t| t.is_resumable())
            .map(|t| StartReq {
                id: t.id,
                name: t.name.clone(),
                link: t.link.clone(),
                path: t.path.clone(),
                segments: t.segments,
            })
            .collect();
        if runnable.is_empty() {
            if !ids.is_empty() {
                self.push_log("选中的任务都在下载中或已完成，无需开始".into());
            }
            return;
        }
        let skipped = ids.len() - runnable.len();
        if skipped > 0 {
            self.push_log(format!("跳过 {skipped} 个正在下载/已完成的任务"));
        }
        // Tasks restored from the saved list have no engine-side spec yet; the
        // worker re-reads their post from the link, then resumes the `.part`.
        let restored = runnable
            .iter()
            .filter(|r| {
                self.tasks
                    .iter()
                    .any(|t| t.id == r.id && t.status == "已停止" && t.done > 0)
            })
            .count();
        self.push_log(if restored > 0 {
            format!(
                "开始 {} 个任务（{} 个需要重新连接，都已下载的部分会续传）",
                runnable.len(),
                restored
            )
        } else {
            format!("开始 {} 个任务（已下载的部分会续传，不会重下）", runnable.len())
        });
        // Optimistic flip: a restored task stays "已停止" until the engine has
        // re-read its post, and clicking 开始 twice in that window would queue
        // the same file twice.
        for r in &runnable {
            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == r.id) {
                t.status = "排队中".into();
            }
        }
        self.touch_tasks_now();
        self.tg_send(TgCmd::Start {
            reqs: runnable,
            files: self.settings.files as usize,
        });
    }

    fn stop(&mut self, ids: Vec<u64>) {
        let live: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| self.task(*id).is_some_and(|t| t.is_active()))
            .collect();
        if live.is_empty() {
            if !ids.is_empty() {
                self.push_log("选中的任务都已经停了".into());
            }
            return;
        }
        self.push_log(format!("停止 {} 个任务（进度已保存）", live.len()));
        self.tg_send(TgCmd::Stop(live));
    }

    fn remove(&mut self, ids: Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        // Do not leave an orphan transfer running in the engine after its row
        // is gone; stop first, then drop the row.
        let live: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| self.task(*id).is_some_and(|t| t.is_active()))
            .collect();
        if !live.is_empty() {
            self.tg_send(TgCmd::Stop(live));
        }
        self.tasks.retain(|t| !ids.contains(&t.id));
        for id in &ids {
            self.selected.remove(id);
            if self.current == Some(*id) {
                self.current = self.tasks.first().map(|t| t.id);
            }
        }
        self.push_log("已从列表移除（磁盘上的文件不会被删除）".into());
        // Removing a row produces no engine event, so nothing would mark the
        // list dirty on its own.
        self.touch_tasks_now();
    }

    /// Reveals a file, or opens a folder, in Explorer.
    ///
    /// A task that is still downloading has no final file on disk yet — only
    /// `xxx.mp4.part` — so handing its target path straight to `explorer` made
    /// Explorer open "此电脑" or whatever folder it last remembered, which is
    /// why 「打开文件夹」 looked like it opened the wrong place. Resolve to
    /// something that actually exists first, and say what got opened.
    fn open_path(&mut self, path: &str) {
        let mut p = PathBuf::from(path.trim());
        if p.as_os_str().is_empty() {
            self.push_log("路径是空的，没法打开".into());
            return;
        }
        if p.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                p = cwd.join(&p);
            }
        }
        let label = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.trim().to_string());

        if p.is_dir() {
            open_folder(&p);
            return;
        }
        if p.is_file() {
            select_in_folder(&p);
            return;
        }
        // Still downloading: the only thing on disk is the temp file, and
        // pointing Explorer at it is more useful than opening the bare folder.
        let part = PathBuf::from(format!("{}.part", p.display()));
        if part.is_file() {
            select_in_folder(&part);
            self.push_log(format!("{label} 还没下完，已在文件夹里选中它的 .part 临时文件"));
            return;
        }
        // Nothing there yet (queued / stopped / failed / wrong path): fall back
        // to the nearest existing parent instead of letting Explorer guess.
        match p.ancestors().skip(1).find(|d| d.is_dir()) {
            Some(d) => {
                open_folder(d);
                self.push_log(format!(
                    "{label} 还不存在，已打开它所在的目录 {}",
                    d.display()
                ));
            }
            None => self.push_log(format!(
                "找不到 {path} 对应的目录，可能已经被删掉或移走了"
            )),
        }
    }
}

/// Opens a folder in Explorer. `arg` lets Rust apply the quoting the path needs.
fn open_folder(dir: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
}

/// `explorer /select,"<path>"`: the quotes must wrap **only** the path.
///
/// Letting `Command` quote the whole argument on its own yields
/// `"/select,C:\a b\x.mp4"`, which Explorer does not parse — it silently opens
/// the default folder instead. `raw_arg` lets us place the quotes ourselves.
fn select_in_folder(file: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("explorer.exe");
    cmd.raw_arg(format!("/select,\"{}\"", file.display()));
    let _ = cmd.spawn();
}

impl From<TaskInfo> for Task {
    fn from(i: TaskInfo) -> Self {
        Self {
            id: i.id,
            name: i.name,
            size: i.size,
            done: 0,
            status: "排队中".into(),
            speed: 0.0,
            conns: 0,
            link: i.link,
            path: i.path,
            segments: i.segments,
        }
    }
}

// ---------------------------------------------------------------- widgets

fn nav_item(ui: &mut egui::Ui, label: &str, selected: bool, p: &Palette) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 34.0), egui::Sense::click());
    let bg_rect = rect.shrink2(egui::vec2(10.0, 2.0));
    let bg = if selected {
        p.sel
    } else if resp.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(bg_rect, cr(8), bg);
    }
    let color = if selected { p.accent } else { p.text };
    ui.painter().text(
        bg_rect.left_center() + egui::vec2(14.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        color,
    );
    resp
}

/// A fixed-width column inside a table row.
///
/// The rect is reserved up front: letting the child size itself would collapse
/// every column to the width of its label and the table would lose its grid.
fn cell(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    add(&mut child);
}

/// One table row: paints its own background, then lays the cells out inside it.
fn row_band(
    ui: &mut egui::Ui,
    p: &Palette,
    height: f32,
    base: Color32,
    selected: bool,
    sense: egui::Sense,
    build: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let bg = if selected {
        p.sel
    } else if resp.hovered() {
        p.hover
    } else {
        base
    };
    if bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, cr(6), bg);
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(10.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    build(&mut child);
    resp
}

const W_CHECK: f32 = 26.0;
const W_SIZE: f32 = 88.0;
const W_PROG: f32 = 152.0;
const W_STATUS: f32 = 104.0;
const W_SPEED: f32 = 92.0;
const W_ETA: f32 = 86.0;
const W_CONN: f32 = 58.0;
const W_DATE: f32 = 108.0;
const W_NOTE: f32 = 236.0;

// ---------------------------------------------------------------- views

impl App {
    fn status_color(&self, t: &Task, p: &Palette) -> Color32 {
        if t.is_done() {
            p.ok
        } else if t.is_fail() {
            p.err
        } else if t.status == "已停止" {
            p.warn
        } else if t.status == "下载中" {
            p.accent
        } else {
            p.weak
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(RichText::new("TGVD").size(20.0).strong().color(p.text));
            ui.label(RichText::new("电报下载器").size(11.5).color(p.weak));
        });
        ui.add_space(14.0);

        let items = [
            (Page::Search, "搜索"),
            (Page::Downloads, "下载"),
            (Page::Logs, "日志"),
            (Page::Settings, "设置"),
        ];
        for (page, label) in items {
            if nav_item(ui, label, self.page == page, &p).clicked() {
                self.page = page;
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                let (color, name) = if self.logged_in() {
                    (p.ok, self.tg_user.clone())
                } else {
                    (p.weak, "未登录".to_string())
                };
                ui.label(RichText::new("●").size(9.0).color(color));
                ui.label(RichText::new(name).size(12.0).color(p.weak));
            });
            let speed: f64 = self
                .tasks
                .iter()
                .filter(|t| t.is_active())
                .map(|t| t.speed)
                .sum();
            let conns: u32 = self.tasks.iter().map(|t| t.conns).sum();
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(
                    RichText::new(format!(
                        "{} · {} 连接",
                        tgvd::format_bytes(speed as u64),
                        conns
                    ))
                    .size(11.5)
                    .color(p.weak),
                );
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(
                    RichText::new(format!("{} 个任务", self.tasks.len()))
                        .size(11.5)
                        .color(p.weak),
                );
            });
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        let title = match self.page {
            Page::Search => "搜索频道",
            Page::Downloads => "全部下载",
            Page::Logs => "日志",
            Page::Settings => "设置",
        };
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.label(RichText::new(title).size(20.0).strong().color(p.text));
            if self.page == Page::Downloads {
                ui.label(
                    RichText::new(format!("{} 项", self.tasks.len()))
                        .size(12.5)
                        .color(p.weak),
                );
                if self.resolving {
                    ui.spinner();
                    ui.label(RichText::new("正在解析链接…").size(12.0).color(p.weak));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("添加任务").color(Color32::WHITE))
                            .fill(p.accent)
                            .corner_radius(cr(8))
                            .min_size(egui::vec2(88.0, 28.0)),
                    )
                    .clicked()
                {
                    self.add_open = true;
                }
                if self.page == Page::Downloads {
                    if ui
                        .add_enabled(
                            self.tasks.iter().any(|t| t.is_active()),
                            egui::Button::new("全部停止").corner_radius(cr(8)),
                        )
                        .clicked()
                    {
                        self.tg_send(TgCmd::StopAll);
                        self.push_log("已请求停止全部任务".into());
                    }
                    if ui
                        .add_enabled(
                            self.tasks.iter().any(|t| t.is_resumable()),
                            egui::Button::new("全部开始").corner_radius(cr(8)),
                        )
                        .on_hover_text("把所有已停止 / 失败 / 未完成的任务重新排进队列并续传")
                        .clicked()
                    {
                        let ids: Vec<u64> = self
                            .tasks
                            .iter()
                            .filter(|t| t.is_resumable())
                            .map(|t| t.id)
                            .collect();
                        self.start(ids);
                    }
                    if ui
                        .add_enabled(
                            self.tasks.iter().any(|t| t.is_done()),
                            egui::Button::new("清除已完成").corner_radius(cr(8)),
                        )
                        .clicked()
                    {
                        let ids: Vec<u64> = self
                            .tasks
                            .iter()
                            .filter(|t| t.is_done())
                            .map(|t| t.id)
                            .collect();
                        self.remove(ids);
                    }
                }
            });
        });
    }

    // --- search page ----------------------------------------------------

    fn search_form(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        ui.horizontal(|ui| {
            ui.label(RichText::new("频道").size(12.0).color(p.weak));
            ui.add(
                egui::TextEdit::singleline(&mut self.search_input)
                    .desired_width(300.0)
                    .hint_text("@频道名 或 https://t.me/xxxx"),
            );
            ui.add_space(6.0);
            ui.label(RichText::new("关键字").size(12.0).color(p.weak));
            let kw = ui.add(
                egui::TextEdit::singleline(&mut self.search_kw)
                    .desired_width(190.0)
                    .hint_text("留空＝只看最新的"),
            );
            let enter = kw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add_enabled(
                    !self.searching,
                    egui::Button::new(RichText::new("搜索").color(Color32::WHITE))
                        .fill(p.accent)
                        .corner_radius(cr(8))
                        .min_size(egui::vec2(72.0, 28.0)),
                )
                .clicked()
                || enter
            {
                self.start_search();
            }
            if self.searching {
                ui.spinner();
            }
        });
        ui.add_space(3.0);
        let text = if self.searching {
            "正在向 Telegram 请求，大频道可能要几秒…".to_string()
        } else if self.search_note.is_empty() {
            "关键字由 Telegram 服务端匹配，能翻到该频道的全部历史；不填就只读最近的消息。"
                .to_string()
        } else {
            self.search_note.clone()
        };
        ui.label(RichText::new(text).size(11.5).color(p.weak));
    }

    fn search_view(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        self.search_form(ui);
        ui.add_space(6.0);

        let width = ui.available_width();
        let name_w = (width - (W_CHECK + W_SIZE + W_DATE + W_NOTE + 4.0 * 8.0 + 20.0)).max(150.0);

        let hits = self.hits.clone();
        row_band(ui, &p, 32.0, p.panel, false, egui::Sense::hover(), |ui| {
            let all = !hits.is_empty() && self.hit_sel.len() == hits.len();
            let mut checked = all;
            cell(ui, W_CHECK, |ui| {
                if ui.checkbox(&mut checked, "").clicked() {
                    if checked {
                        self.hit_sel = hits.iter().map(|h| h.idx).collect();
                    } else {
                        self.hit_sel.clear();
                    }
                }
            });
            let head = |ui: &mut egui::Ui, text: &str| {
                ui.label(RichText::new(text).size(12.0).color(p.weak));
            };
            cell(ui, name_w, |ui| head(ui, "标题"));
            cell(ui, W_SIZE, |ui| head(ui, "大小"));
            cell(ui, W_DATE, |ui| head(ui, "发布"));
            cell(ui, W_NOTE, |ui| head(ui, "帖子内容"));
        });
        ui.add_space(2.0);

        if self.hits.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(if self.searching {
                        "搜索中…"
                    } else {
                        "还没有搜索结果"
                    })
                    .size(14.0)
                    .color(p.weak),
                );
                if !self.searching {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("填频道和关键字，点「搜索」，勾选要下的视频再点「下载选中」")
                            .size(12.0)
                            .color(p.weak),
                    );
                }
            });
            return;
        }

        let mut toggle: Option<usize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for h in &hits {
                    let selected = self.hit_sel.contains(&h.idx);
                    let resp = row_band(
                        ui,
                        &p,
                        34.0,
                        Color32::TRANSPARENT,
                        selected,
                        egui::Sense::click(),
                        |ui| {
                            let mut checked = selected;
                            cell(ui, W_CHECK, |ui| {
                                if ui.checkbox(&mut checked, "").changed() {
                                    toggle = Some(h.idx);
                                }
                            });
                            cell(ui, name_w, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&h.name).size(13.0).color(p.text),
                                    )
                                    .truncate(),
                                );
                            });
                            cell(ui, W_SIZE, |ui| {
                                ui.label(
                                    RichText::new(if h.size > 0 {
                                        tgvd::format_bytes(h.size)
                                    } else {
                                        "—".into()
                                    })
                                    .size(12.0)
                                    .color(p.weak),
                                );
                            });
                            cell(ui, W_DATE, |ui| {
                                ui.label(RichText::new(&h.date).size(11.5).color(p.weak));
                            });
                            cell(ui, W_NOTE, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(if h.caption.is_empty() {
                                            "（无配文）"
                                        } else {
                                            h.caption.as_str()
                                        })
                                        .size(11.5)
                                        .color(p.weak),
                                    )
                                    .truncate(),
                                );
                            });
                        },
                    );
                    // Clicking anywhere on the row ticks it: this list exists
                    // only to be picked from.
                    if resp.clicked() {
                        toggle = Some(h.idx);
                    }
                    ui.add_space(1.0);
                }
            });
        if let Some(idx) = toggle {
            if !self.hit_sel.remove(&idx) {
                self.hit_sel.insert(idx);
            }
        }
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        let n = self.hit_sel.len();
        let total: u64 = self
            .hits
            .iter()
            .filter(|h| self.hit_sel.contains(&h.idx))
            .map(|h| h.size)
            .sum();
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            if ui
                .add_enabled(
                    !self.hits.is_empty(),
                    egui::Button::new("全选").corner_radius(cr(7)),
                )
                .clicked()
            {
                self.hit_sel = self.hits.iter().map(|h| h.idx).collect();
            }
            if ui
                .add_enabled(n > 0, egui::Button::new("清空选择").corner_radius(cr(7)))
                .clicked()
            {
                self.hit_sel.clear();
            }
            ui.separator();
            ui.label(
                RichText::new(if n > 0 {
                    format!(
                        "已选 {n} / {} 项 · {}",
                        self.hits.len(),
                        tgvd::format_bytes(total)
                    )
                } else {
                    format!("共 {} 项，未选择", self.hits.len())
                })
                .size(11.5)
                .color(p.weak),
            );
            // A right-to-left layout lets the button spill past the panel edge;
            // padding by hand keeps it inside the margin.
            let btn_w = 132.0;
            ui.add_space((ui.available_width() - btn_w).max(4.0));
            if ui
                .add_enabled(
                    n > 0,
                    egui::Button::new(
                        RichText::new(format!("下载选中（{n}）")).color(Color32::WHITE),
                    )
                    .fill(p.accent)
                    .corner_radius(cr(8))
                    .min_size(egui::vec2(btn_w, 28.0)),
                )
                .clicked()
            {
                self.download_picked();
            }
        });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        let width = ui.available_width();
        let name_w = (width
            - (W_CHECK + W_SIZE + W_PROG + W_STATUS + W_SPEED + W_ETA + W_CONN + 7.0 * 8.0 + 20.0))
            .max(140.0);

        // 表头
        row_band(ui, &p, 32.0, p.panel, false, egui::Sense::hover(), |ui| {
            let all = !self.tasks.is_empty() && self.selected.len() == self.tasks.len();
            let mut checked = all;
            cell(ui, W_CHECK, |ui| {
                if ui.checkbox(&mut checked, "").clicked() {
                    if checked {
                        self.selected = self.tasks.iter().map(|t| t.id).collect();
                    } else {
                        self.selected.clear();
                    }
                }
            });
            let head = |ui: &mut egui::Ui, text: &str| {
                ui.label(RichText::new(text).size(12.0).color(p.weak));
            };
            cell(ui, name_w, |ui| head(ui, "名称"));
            cell(ui, W_SIZE, |ui| head(ui, "大小"));
            cell(ui, W_PROG, |ui| head(ui, "进度"));
            cell(ui, W_STATUS, |ui| head(ui, "状态"));
            cell(ui, W_SPEED, |ui| head(ui, "下载速度"));
            cell(ui, W_ETA, |ui| head(ui, "剩余时间"));
            cell(ui, W_CONN, |ui| head(ui, "连接数"));
        });
        ui.add_space(2.0);

        if self.tasks.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("还没有任务")
                        .size(14.0)
                        .color(p.weak),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("点右上角「添加任务」，粘贴 t.me 链接或 @频道名")
                        .size(12.0)
                        .color(p.weak),
                );
            });
            return;
        }

        // 行数据快照，避免与 self.selected 的借用冲突
        let rows: Vec<Task> = self.tasks.clone();
        let mut toggle: Option<u64> = None;
        let mut activate: Option<u64> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for t in &rows {
                    let selected = self.selected.contains(&t.id);
                    let resp = row_band(ui, &p, 34.0, Color32::TRANSPARENT, selected, egui::Sense::click(), |ui| {
                        let mut checked = selected;
                        cell(ui, W_CHECK, |ui| {
                            if ui.checkbox(&mut checked, "").changed() {
                                toggle = Some(t.id);
                            }
                        });
                        cell(ui, name_w, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&t.name).size(13.0).color(p.text),
                                )
                                .truncate(),
                            );
                        });
                        cell(ui, W_SIZE, |ui| {
                            ui.label(
                                RichText::new(if t.size > 0 {
                                    tgvd::format_bytes(t.size)
                                } else {
                                    "—".into()
                                })
                                .size(12.0)
                                .color(p.weak),
                            );
                        });
                        cell(ui, W_PROG, |ui| {
                            if t.size > 0 {
                                let f = t.progress();
                                let color = if t.is_done() {
                                    p.ok
                                } else if t.is_fail() {
                                    p.err
                                } else {
                                    p.accent
                                };
                                ui.add(
                                    egui::ProgressBar::new(f)
                                        .desired_width(W_PROG - 46.0)
                                        .desired_height(10.0)
                                        .fill(color)
                                        .corner_radius(cr(5)),
                                );
                                ui.label(
                                    RichText::new(format!("{:.0}%", f * 100.0))
                                        .size(11.5)
                                        .color(p.weak),
                                );
                            } else {
                                ui.label(RichText::new("—").size(11.5).color(p.weak));
                            }
                        });
                        cell(ui, W_STATUS, |ui| {
                            // Long error texts must not bleed into the next column.
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&t.status)
                                        .size(12.0)
                                        .color(self.status_color(t, &p)),
                                )
                                .truncate(),
                            );
                        });
                        cell(ui, W_SPEED, |ui| {
                            let text = if t.speed > 1.0 {
                                format!("{}/s", tgvd::format_bytes(t.speed as u64))
                            } else {
                                "—".to_string()
                            };
                            ui.label(RichText::new(text).size(12.0).color(p.weak));
                        });
                        cell(ui, W_ETA, |ui| {
                            let text = t.eta().map(format_eta).unwrap_or_else(|| "—".to_string());
                            ui.label(RichText::new(text).size(12.0).color(p.weak));
                        });
                        cell(ui, W_CONN, |ui| {
                            let text = if t.conns > 0 {
                                t.conns.to_string()
                            } else {
                                "—".to_string()
                            };
                            ui.label(RichText::new(text).size(12.0).color(p.weak));
                        });
                    });
                    if resp.clicked() {
                        activate = Some(t.id);
                    }
                    ui.add_space(1.0);
                }
            });

        if let Some(id) = toggle {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
        }
        if let Some(id) = activate {
            self.current = Some(id);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        let ids = self.selected_ids();
        let has = !ids.is_empty();
        // A stopped task must be startable again, otherwise the stop button is a
        // one-way door. Finished downloads are excluded on purpose: queueing one
        // again would put a second writer on the same file.
        let can_start = ids.iter().any(|id| self.task(*id).is_some_and(|t| t.is_resumable()));
        let can_stop = ids.iter().any(|id| self.task(*id).is_some_and(|t| t.is_active()));
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let btn = |ui: &mut egui::Ui, text: &str, enabled: bool| -> bool {
                ui.add_enabled(enabled, egui::Button::new(text).corner_radius(cr(7)))
                    .clicked()
            };
            if ui
                .add_enabled(can_start, egui::Button::new("开始").corner_radius(cr(7)))
                .on_hover_text("开始 / 继续选中的任务，已下载的部分会续传，不会重下")
                .on_disabled_hover_text("选中的任务都在下载中或已完成")
                .clicked()
            {
                self.start(ids.clone());
            }
            if ui
                .add_enabled(can_stop, egui::Button::new("停止").corner_radius(cr(7)))
                .on_hover_text("暂停选中的任务，进度会保存，之后可以继续")
                .on_disabled_hover_text("选中的任务都没有在下载")
                .clicked()
            {
                self.stop(ids.clone());
            }
            if btn(ui, "打开文件夹", has) {
                // `cloned` so the borrow of `self.tasks` ends before `open_path`
                // takes `&mut self`.
                if let Some(t) = self.task(ids[0]).cloned() {
                    self.open_path(&t.path);
                }
            }
            if btn(ui, "复制链接", has) {
                if let Some(t) = self.task(ids[0]) {
                    ui.ctx().copy_text(t.link.clone());
                }
            }
            if btn(ui, "移除", has) {
                self.remove(ids.clone());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(if has {
                        format!("已选 {} 项", ids.len())
                    } else {
                        "未选择任务".to_string()
                    })
                    .size(11.5)
                    .color(p.weak),
                );
            });
        });
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        let Some(t) = self.current.and_then(|id| self.task(id)).cloned() else {
            ui.add_space(10.0);
            ui.label(
                RichText::new("点一行任务查看详情")
                    .size(12.5)
                    .color(p.weak),
            );
            return;
        };

        ui.horizontal(|ui| {
            let chip = egui::Frame::NONE
                .fill(if self.dark {
                    Color32::from_rgb(52, 55, 64)
                } else {
                    Color32::from_rgb(45, 48, 56)
                })
                .corner_radius(cr(5))
                .inner_margin(egui::Margin::symmetric(8, 3));
            chip.show(ui, |ui| {
                ui.label(
                    RichText::new("MTProto")
                        .size(10.5)
                        .strong()
                        .color(Color32::WHITE),
                );
            });
            ui.add(
                egui::Label::new(
                    RichText::new(&t.link).size(12.5).monospace().color(p.text),
                )
                .truncate(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(&t.status)
                        .size(12.0)
                        .color(self.status_color(&t, &p)),
                );
            });
        });

        ui.add_space(6.0);
        let f = t.progress();
        ui.add(
            egui::ProgressBar::new(f)
                .desired_width(ui.available_width() - 4.0)
                .desired_height(8.0)
                .fill(if t.is_done() {
                    p.ok
                } else if t.is_fail() {
                    p.err
                } else {
                    p.accent
                })
                .corner_radius(cr(4)),
        );
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{:.1}% · {} / {}",
                    f * 100.0,
                    tgvd::format_bytes(t.done),
                    if t.size > 0 {
                        tgvd::format_bytes(t.size)
                    } else {
                        "未知".into()
                    }
                ))
                .size(12.0)
                .color(p.weak),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if t.speed > 1.0 {
                    ui.label(
                        RichText::new(format!("{}/s", tgvd::format_bytes(t.speed as u64)))
                            .size(12.0)
                            .color(p.text),
                    );
                }
                if let Some(secs) = t.eta() {
                    ui.label(
                        RichText::new(format!("剩余 {}", format_eta(secs)))
                            .size(12.0)
                            .color(p.weak),
                    );
                }
            });
        });

        ui.add_space(4.0);
        egui::Grid::new("detail_grid")
            .num_columns(2)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("保存到").size(12.0).color(p.weak));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&t.path).size(12.0).color(p.text),
                        )
                        .truncate(),
                    );
                    if ui.small_button("打开").clicked() {
                        let path = t.path.clone();
                        self.open_path(&path);
                    }
                });
                ui.end_row();

                ui.label(RichText::new("文件").size(12.0).color(p.weak));
                ui.add(
                    egui::Label::new(RichText::new(&t.name).size(12.0).color(p.text))
                        .truncate(),
                );
                ui.end_row();

                ui.label(RichText::new("连接数").size(12.0).color(p.weak));
                ui.label(
                    RichText::new(format!(
                        "{}{}",
                        t.conns,
                        if t.is_active() { " 条正在传输" } else { "" }
                    ))
                    .size(12.0)
                    .color(p.text),
                );
                ui.end_row();
            });
    }

    fn logs_view(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("运行日志")
                    .size(13.0)
                    .strong()
                    .color(p.text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清空").clicked() {
                    self.logs.clear();
                }
                if ui.button("打开日志文件").clicked() {
                    let path = telegram::session_path()
                        .with_file_name("tgvd.log")
                        .to_string_lossy()
                        .to_string();
                    self.open_path(&path);
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.logs {
                    ui.label(
                        RichText::new(line)
                            .size(12.0)
                            .monospace()
                            .color(p.text),
                    );
                }
            });
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.dark);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // ---- account
                ui.label(RichText::new("账号").size(14.0).strong().color(p.text));
                ui.label(
                    RichText::new("所有下载都通过你的 Telegram 账号进行，登录一次即可（凭证换版本不丢）。")
                        .size(12.0)
                        .color(p.weak),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("状态").size(12.0).color(p.weak));
                    let (color, text) = if self.logged_in() {
                        (p.ok, format!("已登录 · {}", self.tg_user))
                    } else {
                        (p.weak, self.tg_stage.clone())
                    };
                    ui.label(RichText::new(text).size(12.5).color(color));
                    if self.tg_pending() {
                        let secs = self
                            .tg_started
                            .map(|t| t.elapsed().as_secs_f64())
                            .unwrap_or(0.0);
                        ui.label(
                            RichText::new(format!("{secs:.1}s"))
                                .monospace()
                                .size(11.5)
                                .color(p.weak),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("手机号").size(12.0).color(p.weak));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.tg_phone)
                            .desired_width(200.0)
                            .hint_text("+8613800138000"),
                    );
                    if ui.button("登录 / 重新检查").clicked() {
                        let api_id = self
                            .tg_api_id
                            .trim()
                            .parse::<i32>()
                            .unwrap_or(telegram::DEFAULT_API_ID);
                        let hash = self.tg_api_hash.trim();
                        let api_hash = if hash.is_empty() {
                            telegram::DEFAULT_API_HASH.to_string()
                        } else {
                            hash.to_string()
                        };
                        let phone = self.tg_phone.trim().to_string();
                        let has_session =
                            std::path::Path::new(&self.session_file).exists();
                        if phone.is_empty() && !has_session {
                            self.push_log(
                                "请先填手机号（国际格式，如 +8613800138000）".into(),
                            );
                        } else {
                            self.tg_need = TgNeed::None;
                            self.tg_stage = "正在连接…".into();
                            self.tg_send(TgCmd::Login {
                                api_id,
                                api_hash,
                                phone,
                            });
                        }
                    }
                    if ui
                        .add_enabled(self.logged_in(), egui::Button::new("退出登录"))
                        .clicked()
                    {
                        self.tg_send(TgCmd::Logout);
                        self.tg_stage = "正在退出登录…".into();
                    }
                });
                if self.tg_need == TgNeed::Code {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("验证码").size(12.0).color(p.weak));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.tg_code)
                                .desired_width(120.0),
                        );
                        if ui.button("提交").clicked() {
                            let code = self.tg_code.trim().to_string();
                            if !code.is_empty() {
                                self.tg_need = TgNeed::None;
                                self.tg_send(TgCmd::Code(code));
                            }
                        }
                    });
                } else if self.tg_need == TgNeed::Password {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("两步密码").size(12.0).color(p.weak));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.tg_pw)
                                .password(true)
                                .desired_width(160.0),
                        );
                        if ui.button("提交").clicked() {
                            let pw = self.tg_pw.clone();
                            if !pw.is_empty() {
                                self.tg_need = TgNeed::None;
                                self.tg_send(TgCmd::Password(pw));
                            }
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("API").size(12.0).color(p.weak));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.tg_api_id)
                            .desired_width(80.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.tg_api_hash)
                            .desired_width(300.0),
                    );
                    if ui.small_button("恢复默认").clicked() {
                        self.tg_api_id = telegram::DEFAULT_API_ID.to_string();
                        self.tg_api_hash = telegram::DEFAULT_API_HASH.to_string();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("凭证").size(12.0).color(p.weak));
                    ui.add(
                        egui::Label::new(
                            RichText::new(&self.session_file)
                                .size(11.5)
                                .monospace()
                                .color(p.weak),
                        )
                        .truncate(),
                    );
                    if ui.small_button("目录").clicked() {
                        if let Some(dir) =
                            std::path::Path::new(&self.session_file).parent()
                        {
                            let d = dir.to_string_lossy().to_string();
                            self.open_path(&d);
                        }
                    }
                });

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);

                // ---- download
                ui.label(RichText::new("下载").size(14.0).strong().color(p.text));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("保存到").size(12.0).color(p.weak))
                        .on_hover_text(
                            "改完之后新加入队列的任务才用新目录；已经在列表里的任务还是原来那个",
                        );
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.settings.out_dir)
                                .desired_width(420.0),
                        )
                        .changed()
                    {
                        self.touch_settings();
                    }
                    if ui.button("选择…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.settings.out_dir = dir.to_string_lossy().to_string();
                            self.touch_settings();
                        }
                    }
                    if ui.button("打开").clicked() {
                        let d = self.settings.out_dir.trim().to_string();
                        // The folder is only created when the first download
                        // starts, so a brand-new path used to fall back to its
                        // parent. Create it here instead.
                        if !d.is_empty() && !std::path::Path::new(&d).exists() {
                            if let Err(e) = std::fs::create_dir_all(&d) {
                                self.push_log(format!("创建目录 {d} 失败：{e}"));
                            }
                        }
                        self.open_path(&d);
                    }
                });
                egui::Grid::new("dl_grid")
                    .num_columns(2)
                    .spacing([14.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("同时下载").size(12.0).color(p.weak));
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.settings.files, 1..=8)
                                        .suffix(" 个文件"),
                                )
                                .changed()
                            {
                                self.touch_settings();
                                // Push it to the live engine right away: the
                                // slider used to only reach the engine the next
                                // time something was queued, so changing it
                                // mid-download did nothing at all.
                                let n = self.settings.files as usize;
                                self.tg_send_quiet(TgCmd::SetFiles { files: n });
                            }
                            ui.label(
                                RichText::new("几个文件同时下载")
                                    .size(11.5)
                                    .color(p.weak),
                            );
                        });
                        ui.end_row();

                        ui.label(RichText::new("单文件连接").size(12.0).color(p.weak));
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.settings.segments, 1..=16)
                                        .suffix(" 段"),
                                )
                                .changed()
                            {
                                self.touch_settings();
                            }
                            ui.label(
                                RichText::new("一个文件切几段并行拉")
                                    .size(11.5)
                                    .color(p.weak),
                            );
                        });
                        ui.end_row();

                        ui.label(RichText::new("每频道最多").size(12.0).color(p.weak));
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.settings.limit, 5..=200)
                                        .suffix(" 条"),
                                )
                                .changed()
                            {
                                self.touch_settings();
                            }
                            ui.label(
                                RichText::new("只贴频道链接时，最多抓最近多少条")
                                    .size(11.5)
                                    .color(p.weak),
                            );
                        });
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut self.settings.overwrite, "覆盖已存在的文件")
                        .changed()
                    {
                        self.touch_settings();
                    }
                    let mut dark = self.dark;
                    if ui.checkbox(&mut dark, "深色界面").changed() {
                        self.dark = dark;
                        self.settings.dark = dark;
                        self.touch_settings();
                    }
                });
                ui.label(
                    RichText::new(
                        "「同时下载」指几个文件并行，拖动后立即生效（已经在下的文件跑完才让位）；「单文件连接」指一个文件切几段同时拉，只对之后加入队列的任务生效。两个都调高更快，也更吃带宽。",
                    )
                    .size(11.5)
                    .color(p.weak),
                );

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(RichText::new("关于").size(14.0).strong().color(p.text));
                ui.label(
                    RichText::new(format!(
                        "TGVD {} · 只走 Telegram 账号通道（MTProto），支持多任务并行、断点续传；下载中写 *.part，全部到齐才改名。",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .size(11.5)
                    .color(p.weak),
                );
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let got_events = self.pump();
        self.flush_settings();
        self.flush_tasks();

        if self.dark != palette_is_dark(ctx) {
            // 主题切换后立即重建样式，避免拖到下一帧才有反馈
            apply_theme(ctx, self.dark);
        }

        let active = self.tasks.iter().any(|t| t.is_active())
            || self.resolving
            || self.searching
            || self.tg_pending();
        if got_events || active {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        let p = palette(self.dark);

        egui::SidePanel::left("nav")
            .exact_width(180.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(0, 6)),
            )
            .show(ctx, |ui| self.sidebar(ui));

        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ctx, |ui| {
                self.header(ui);
                ui.add_space(6.0);
                let rect = ui.available_rect_before_wrap();
                ui.painter().hline(
                    rect.x_range(),
                    rect.top(),
                    egui::Stroke::new(1.0_f32, p.line),
                );
                ui.add_space(1.0);
            });

        if self.page == Page::Search {
            egui::TopBottomPanel::bottom("search_bar")
                .frame(
                    egui::Frame::NONE
                        .fill(p.panel)
                        .inner_margin(egui::Margin::symmetric(16, 6)),
                )
                .show(ctx, |ui| self.search_bar(ui));
        }

        if self.page == Page::Downloads {
            egui::TopBottomPanel::bottom("detail")
                .resizable(true)
                .default_height(190.0)
                .min_height(120.0)
                .frame(
                    egui::Frame::NONE
                        .fill(p.bg)
                        .inner_margin(egui::Margin::symmetric(18, 10)),
                )
                .show(ctx, |ui| {
                    ui.add_space(2.0);
                    self.details(ui);
                });

            egui::TopBottomPanel::bottom("toolbar")
                .frame(
                    egui::Frame::NONE
                        .fill(p.panel)
                        .inner_margin(egui::Margin::symmetric(16, 6)),
                )
                .show(ctx, |ui| self.toolbar(ui));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ctx, |ui| match self.page {
                Page::Search => self.search_view(ui),
                Page::Downloads => self.table(ui),
                Page::Logs => self.logs_view(ui),
                Page::Settings => self.settings_view(ui),
            });

        // 添加任务窗口
        if self.add_open {
            // `win_open` is what the title-bar X toggles; the closure is not
            // allowed to touch it, so the buttons use a separate `close` flag.
            let mut win_open = true;
            let mut close = false;
            egui::Window::new("添加下载任务")
                .collapsible(false)
                .resizable(true)
                .default_width(620.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .frame(
                    egui::Frame::window(&ctx.style())
                        .fill(p.card)
                        .inner_margin(egui::Margin::same(14)),
                )
                .open(&mut win_open)
                .show(ctx, |ui| {
                    ui.label(
                        RichText::new("粘贴 t.me 链接或 @频道名，每行一个：")
                            .size(12.5)
                            .color(p.text),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut self.add_links)
                            .desired_rows(6)
                            .desired_width(f32::INFINITY)
                            .hint_text(
                                "https://t.me/xgduanju/2942\n@somechannel\nhttps://t.me/c/1234567/89",
                            ),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "这里是“直接下”：粘什么就下什么。想按关键字挑视频，用左侧「搜索」页，可以先看结果再勾选。",
                        )
                        .size(11.5)
                        .color(p.weak),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("保存到 {}", self.settings.out_dir))
                                .size(11.5)
                                .color(p.weak),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("加入队列")
                                                .color(Color32::WHITE),
                                        )
                                        .fill(p.accent)
                                        .corner_radius(cr(7))
                                        .min_size(egui::vec2(88.0, 28.0)),
                                    )
                                    .clicked()
                                {
                                    self.submit_links();
                                    close = true;
                                }
                                if ui.button("取消").clicked() {
                                    close = true;
                                }
                            },
                        );
                    });
                });
            if close || !win_open {
                self.add_open = false;
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Last chance to persist: the throttled writer may still be holding a
        // change made in the final second before the window closed.
        self.settings.save();
        save_tasks(&self.tasks);
    }
}

/// Reads the current visuals to detect an external theme switch.
fn palette_is_dark(ctx: &egui::Context) -> bool {
    ctx.style().visuals.dark_mode
}
