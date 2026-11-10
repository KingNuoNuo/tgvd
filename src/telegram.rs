//! MTProto (not the public web preview).
//!
//! Everything goes through a logged-in account: some channels disable anonymous
//! web preview entirely, and private channels are unreachable any other way. It
//! also means every file is a first-class task the UI can pause, retry and
//! resume, instead of a bare HTTPS link.
//!
//! Everything runs on a background thread with its own tokio runtime; the GUI
//! talks to it through a command channel and receives events back.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use grammers_client::media::Media;
use grammers_client::message::Message;
use grammers_client::{Client, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Telegram Desktop's published API credentials (from the open-source client).
/// Works out of the box; replace with your own from https://my.telegram.org if you prefer.
pub const DEFAULT_API_ID: i32 = 2040;
pub const DEFAULT_API_HASH: &str = "b18441a1ff607e10a989891a5462e627";

/// How long any single login-related round trip may take before we call it a
/// dead network. Windows drops an unanswered SYN after ~21 s, so without this
/// the UI would sit on "正在检查登录状态…" for half a minute with no feedback.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- session location

/// Where the login (MTProto auth key) is kept.
///
/// Deliberately **not** next to the executable: replacing the exe with a newer
/// build — or running it from a different folder — used to lose the session and
/// force a fresh SMS code every time. `%APPDATA%\TGVD\tgvd.session` stays put
/// across upgrades.
pub fn session_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|p| PathBuf::from(p).join("AppData").join("Roaming"))
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join("TGVD").join("tgvd.session")
}

/// A release build has no console and the GUI keeps only the last 300 lines, so
/// login diagnostics also go to `%APPDATA%\TGVD\tgvd.log`. Every line carries
/// the milliseconds since start: that is what tells "slow network" apart from
/// "the app itself is slow".
pub fn file_log(msg: &str) {
    use std::io::Write;
    static START: OnceLock<Instant> = OnceLock::new();
    let path = session_path().with_file_name("tgvd.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ms = START.get_or_init(Instant::now).elapsed().as_millis();
        let _ = writeln!(f, "[+{ms:>6}ms] {msg}");
    }
}

/// Places older builds (which kept the session beside the exe) may have used.
fn legacy_session_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("tgvd.session"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("tgvd.session"));
    }
    out
}

/// Resolves the session path, copying a session left by an older build into the
/// new location the first time so an existing login keeps working.
pub fn prepare_session() -> PathBuf {
    prepare_session_at(&session_path())
}

fn prepare_session_at(dst: &Path) -> PathBuf {
    prepare_session_with(dst, &legacy_session_paths())
}

fn prepare_session_with(dst: &Path, candidates: &[PathBuf]) -> PathBuf {
    if dst.exists() {
        return dst.to_path_buf();
    }
    for old in candidates {
        if old == dst || !old.exists() {
            continue;
        }
        if let Some(dir) = dst.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::copy(old, dst).is_ok() {
            return dst.to_path_buf();
        }
    }
    dst.to_path_buf()
}

// ---------------------------------------------------------------- commands

pub enum Cmd {
    Login {
        api_id: i32,
        api_hash: String,
        phone: String,
    },
    Code(String),
    Password(String),
    /// Queues every video behind these links. Returns immediately: videos are
    /// reported one by one as `Ev::Added` and then downloaded in parallel, so a
    /// channel with 30 files becomes 30 independent tasks the UI can manage.
    Add {
        links: Vec<String>,
        limit: usize,
        out_dir: PathBuf,
        overwrite: bool,
        /// How many files may download at the same time.
        files: usize,
        /// How many parallel connections one file uses.
        segments: usize,
        /// Optional server-side keyword: when set, the channel is searched
        /// instead of walking its most recent messages.
        keyword: String,
    },
    /// Re-queue finished/failed tasks; a partially written file resumes.
    Retry(Vec<u64>),
    /// Cancel these tasks (the `.part` file is kept for a later resume).
    Stop(Vec<u64>),
    StopAll,
    /// Reads a channel — or searches inside it when `keyword` is set — and
    /// reports every hit as `Ev::SearchHit`. Nothing is downloaded yet: the user
    /// ticks the ones they want and sends `DownloadPicked`, so a big channel is
    /// browsed instead of blindly queued.
    Search {
        input: String,
        keyword: String,
        limit: usize,
    },
    /// Queues the results the user ticked, by their index in the last search.
    DownloadPicked {
        picks: Vec<usize>,
        out_dir: PathBuf,
        overwrite: bool,
        files: usize,
        segments: usize,
    },
    /// Forget the saved credentials (server-side log out + delete the file).
    Logout,
}

/// One row of the search result list.
#[derive(Clone)]
pub struct SearchHit {
    /// Index into the worker's result set, used as the tick target.
    pub idx: usize,
    pub name: String,
    pub size: u64,
    /// Already formatted in the local time zone.
    pub date: String,
    /// Post text, flattened, for telling near-identical titles apart.
    pub caption: String,
    pub link: String,
}

/// A task as the UI needs to display it.
pub struct TaskInfo {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub link: String,
    pub path: String,
}

pub enum Ev {
    Log(String),
    NeedCode,
    NeedPassword(String),
    Authorized(String),
    LoggedOut,
    Stage(String),
    /// A new row for the task list.
    Added(TaskInfo),
    Progress {
        id: u64,
        done: u64,
        /// Bytes/s for this task alone.
        speed: f64,
        /// Live connection count (segments still transferring).
        conns: u32,
    },
    /// Intermediate state text ("排队中" / "下载中" / "已停止").
    Status {
        id: u64,
        status: String,
    },
    /// Terminal state: 完成 / 已存在 / 错误 / 已取消.
    Done {
        id: u64,
        status: String,
    },
    /// No more `Added` events are coming for the batch that just finished.
    ResolveDone,
    /// A search started; the previous result list is now stale.
    SearchStart,
    SearchHit(SearchHit),
    SearchDone {
        total: usize,
    },
    SearchFail(String),
}

pub struct Worker {
    pub tx: UnboundedSender<Cmd>,
}

impl Worker {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

/// Starts the background MTProto worker. It stays idle until the first `Cmd::Login`.
pub fn spawn(ev: mpsc::Sender<Ev>, session_path: PathBuf) -> Worker {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ev.send(Ev::Log(format!("初始化异步运行时失败: {e}")));
                return;
            }
        };
        rt.block_on(run(ev, rx, session_path));
    });
    Worker { tx }
}

async fn run(ev: mpsc::Sender<Ev>, mut rx: UnboundedReceiver<Cmd>, session_path: PathBuf) {
    let mut client: Option<Client> = None;
    let mut session: Option<Arc<JsonSession>> = None;
    let mut login_token: Option<grammers_client::client::LoginToken> = None;
    let mut password_token: Option<grammers_client::client::PasswordToken> = None;
    // Created on the first `Add`; owns the download queue and the task registry.
    let mut engine: Option<Arc<Engine>> = None;
    // Results of the last search, kept here so the UI only has to send indices
    // back. Replaced on every new search.
    let search: Arc<Mutex<Vec<Item>>> = Arc::new(Mutex::new(Vec::new()));

    let log = |s: String| {
        file_log(&s);
        let _ = ev.send(Ev::Log(s));
    };
    let stage = |s: &str| {
        file_log(s);
        let _ = ev.send(Ev::Stage(s.to_string()));
    };

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Login {
                api_id,
                api_hash,
                phone,
            } => {
                let t0 = Instant::now();
                if client.is_none() {
                    stage("正在连接 Telegram…");
                    let s = Arc::new(JsonSession::open(&session_path));
                    let SenderPool {
                        runner, handle, ..
                    } = SenderPool::new(s.clone(), api_id);
                    let c = Client::new(handle);
                    tokio::spawn(runner.run());
                    client = Some(c);
                    session = Some(s);
                    // SenderPool::new only builds the channels; the socket is
                    // opened lazily by the first request, so this number is not
                    // the connect time.
                    log(format!(
                        "已初始化 MTProto 客户端（api_id {api_id}，{} ms），连接在下一步建立",
                        t0.elapsed().as_millis()
                    ));
                }
                let c = client.as_ref().unwrap().clone();
                stage("正在校验登录状态…");
                let t1 = Instant::now();
                // `is_authorized` is `updates.getState` on a fresh socket, i.e. TCP
                // connect + MTProto handshake + `help.getConfig` + the request
                // itself. That is ~4 round trips: fast on a healthy link, but if
                // the route to the DC is blocked it never returns on its own.
                let probe = tokio::time::timeout(LOGIN_TIMEOUT, c.is_authorized()).await;
                match probe {
                    Err(_) => {
                        log(format!(
                            "校验登录状态超过 {} 秒无响应：到 Telegram 的直连（不走系统代理）可能被阻断。换网络或全局代理后重试",
                            LOGIN_TIMEOUT.as_secs()
                        ));
                        stage("检查登录超时");
                    }
                    Ok(Ok(true)) => {
                        log(format!(
                            "登录状态校验通过（{} ms）",
                            t1.elapsed().as_millis()
                        ));
                        // A stored name saves the `users.getFullUser` round trip
                        // that would otherwise double the wait on start-up.
                        let who = match session.as_ref().and_then(|s| s.cached_name()) {
                            Some(cached) => cached,
                            None => display_user(&c).await,
                        };
                        if let Some(s) = &session {
                            s.set_cached_name(&who);
                            s.flush();
                        }
                        log(format!(
                            "恢复登录完成，总耗时 {} ms",
                            t0.elapsed().as_millis()
                        ));
                        let _ = ev.send(Ev::Authorized(who));
                    }
                    // Empty phone = "resume the saved session"; there is nothing
                    // to resume, so ask for a phone number instead of sending one.
                    Ok(Ok(false)) if phone.trim().is_empty() => {
                        log("没有可复用的登录状态，请填手机号后点「登录」".into());
                        stage("未登录（登录后可下受限 / 私有频道）");
                    }
                    Ok(Ok(false)) => {
                        stage("正在发送验证码…");
                        let sent = tokio::time::timeout(
                            LOGIN_TIMEOUT,
                            c.request_login_code(&phone, &api_hash),
                        )
                        .await;
                        match sent {
                            Err(_) => {
                                log(format!(
                                    "发送验证码超时（{} 秒），网络不通",
                                    LOGIN_TIMEOUT.as_secs()
                                ));
                                stage("登录失败");
                            }
                            Ok(Ok(token)) => {
                                login_token = Some(token);
                                log("验证码已发送到你的 Telegram / 短信".into());
                                let _ = ev.send(Ev::NeedCode);
                            }
                            Ok(Err(e)) => {
                                log(format!("发送验证码失败: {e}"));
                                stage("登录失败");
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        log(format!("检查登录状态失败: {e}"));
                        stage("检查登录失败");
                    }
                }
            }

            Cmd::Code(code) => {
                let Some(token) = login_token.take() else {
                    log("没有待验证的登录，请先点「登录」".into());
                    continue;
                };
                let Some(c) = client.clone() else {
                    log("还没连接 Telegram，请先点「登录」".into());
                    continue;
                };
                stage("正在校验验证码…");
                let t = Instant::now();
                match c.sign_in(&token, code.trim()).await {
                    Ok(_) => {
                        log(format!("验证码校验通过（{} ms）", t.elapsed().as_millis()));
                        let who = display_user(&c).await;
                        if let Some(s) = &session {
                            s.set_cached_name(&who);
                            s.flush();
                        }
                        let _ = ev.send(Ev::Authorized(who));
                    }
                    Err(SignInError::PasswordRequired(pt)) => {
                        let hint = pt.hint().unwrap_or("").to_string();
                        password_token = Some(pt);
                        let _ = ev.send(Ev::NeedPassword(hint));
                    }
                    Err(SignInError::InvalidCode) => {
                        login_token = Some(token);
                        log("验证码错误或已过期，请重试".into());
                        let _ = ev.send(Ev::NeedCode);
                    }
                    Err(SignInError::SignUpRequired) => {
                        log("该手机号还没有 Telegram 账号，请先用官方客户端注册".into());
                        let _ = ev.send(Ev::Stage("需要先注册账号".into()));
                    }
                    Err(e) => log(format!("登录失败: {e}")),
                }
            }

            Cmd::Password(pw) => {
                let Some(pt) = password_token.take() else {
                    log("没有待验证的两步密码".into());
                    continue;
                };
                let Some(c) = client.clone() else {
                    log("还没连接 Telegram，请先点「登录」".into());
                    continue;
                };
                stage("正在校验两步验证密码…");
                match c.check_password(pt, pw.trim()).await {
                    Ok(_) => {
                        let who = display_user(&c).await;
                        if let Some(s) = &session {
                            s.set_cached_name(&who);
                            s.flush();
                        }
                        let _ = ev.send(Ev::Authorized(who));
                    }
                    Err(SignInError::InvalidPassword(pt2)) => {
                        password_token = Some(pt2);
                        log("两步验证密码错误".into());
                        let _ = ev.send(Ev::NeedPassword(String::new()));
                    }
                    Err(e) => log(format!("两步验证失败: {e}")),
                }
            }

            Cmd::Add {
                links,
                limit,
                out_dir,
                overwrite,
                files,
                segments,
                keyword,
            } => {
                let Some(c) = client.clone() else {
                    log("请先在「设置」里登录账号".into());
                    let _ = ev.send(Ev::ResolveDone);
                    continue;
                };
                if !matches!(c.is_authorized().await, Ok(true)) {
                    log("账号还没登录成功，请先在「设置」里完成登录".into());
                    let _ = ev.send(Ev::ResolveDone);
                    continue;
                }
                let eng = match &engine {
                    Some(e) => {
                        e.set_files(files);
                        e.clone()
                    }
                    None => {
                        let e = Arc::new(Engine::new(c.clone(), ev.clone(), files));
                        engine = Some(e.clone());
                        e
                    }
                };
                let cfg = DlCfg {
                    out_dir,
                    overwrite,
                    segments: segments.clamp(1, MAX_SEGMENTS),
                    keyword,
                };
                log(format!("新任务：{} 个链接", links.len()));
                tokio::spawn(async move {
                    resolve_and_queue(eng.clone(), links, limit.max(1), cfg).await;
                    let _ = eng.ev.send(Ev::ResolveDone);
                });
            }

            Cmd::Search {
                input,
                keyword,
                limit,
            } => {
                let Some(c) = client.clone() else {
                    let msg = "请先在「设置」里登录账号".to_string();
                    log(msg.clone());
                    let _ = ev.send(Ev::SearchFail(msg));
                    continue;
                };
                // Only the channel matters here: a `.../123` link still means
                // "browse (or search) this channel", not "download post 123".
                let target = match parse_target(&input) {
                    Ok(t) => Target {
                        channel: t.channel,
                        post: None,
                    },
                    Err(e) => {
                        log(e.clone());
                        let _ = ev.send(Ev::SearchFail(e));
                        continue;
                    }
                };
                let kw = keyword.trim().to_string();
                if let Ok(mut g) = search.lock() {
                    g.clear();
                }
                let _ = ev.send(Ev::SearchStart);
                log(if kw.is_empty() {
                    format!("正在读取 @{} 最近的视频…", target.channel)
                } else {
                    format!("正在 @{} 里搜索「{kw}」…", target.channel)
                });
                let slot = search.clone();
                let ev2 = ev.clone();
                // Resolving can take a while on a big channel; keep the command
                // loop free so a second search can supersede this one.
                tokio::spawn(async move {
                    match collect_videos(&c, &target, limit.max(1), &kw, &ev2).await {
                        Ok(list) => {
                            let hits: Vec<SearchHit> = list
                                .iter()
                                .enumerate()
                                .map(|(idx, it)| SearchHit {
                                    idx,
                                    name: it.name.clone(),
                                    size: it.size,
                                    date: it.date.clone(),
                                    caption: it.caption.clone(),
                                    link: it.link.clone(),
                                })
                                .collect();
                            let total = hits.len();
                            if let Ok(mut g) = slot.lock() {
                                *g = list;
                            }
                            for h in hits {
                                let _ = ev2.send(Ev::SearchHit(h));
                            }
                            let _ = ev2.send(Ev::SearchDone { total });
                        }
                        Err(e) => {
                            let _ = ev2.send(Ev::Log(e.clone()));
                            let _ = ev2.send(Ev::SearchFail(e));
                        }
                    }
                });
            }

            Cmd::DownloadPicked {
                picks,
                out_dir,
                overwrite,
                files,
                segments,
            } => {
                let Some(c) = client.clone() else {
                    log("请先在「设置」里登录账号".into());
                    continue;
                };
                // Results live in the worker, so a tick only costs an index. The
                // media handles are cloned out here and never mutated.
                let items: Vec<Item> = match search.lock() {
                    Ok(g) => picks.iter().filter_map(|i| g.get(*i).cloned()).collect(),
                    Err(_) => Vec::new(),
                };
                if items.is_empty() {
                    log("选中的结果已经失效，请重新搜索".into());
                    let _ = ev.send(Ev::ResolveDone);
                    continue;
                }
                let eng = match &engine {
                    Some(e) => {
                        e.set_files(files);
                        e.clone()
                    }
                    None => {
                        let e = Arc::new(Engine::new(c.clone(), ev.clone(), files));
                        engine = Some(e.clone());
                        e
                    }
                };
                let cfg = DlCfg {
                    out_dir,
                    overwrite,
                    segments: segments.clamp(1, MAX_SEGMENTS),
                    keyword: String::new(),
                };
                if let Err(e) = std::fs::create_dir_all(&cfg.out_dir) {
                    log(format!("创建输出目录 {} 失败: {e}", cfg.out_dir.display()));
                }
                log(format!("开始下载选中的 {} 个视频", items.len()));
                for item in items {
                    enqueue_item(&eng, item, &cfg);
                }
                let _ = ev.send(Ev::ResolveDone);
            }

            Cmd::Retry(ids) => match &engine {
                Some(e) => {
                    for id in ids {
                        match e.spec(id) {
                            Some(sp) => {
                                let _ = ev.send(Ev::Status {
                                    id,
                                    status: "排队中".into(),
                                });
                                queue_file(e.clone(), sp);
                            }
                            None => log(format!("任务 {id} 已不在列表里，无法重试")),
                        }
                    }
                }
                None => log("还没有可重试的任务".into()),
            },

            Cmd::Stop(ids) => {
                if let Some(e) = &engine {
                    for id in ids {
                        e.stop(id);
                    }
                }
            }

            Cmd::StopAll => {
                if let Some(e) = &engine {
                    e.stop_all();
                    log("已请求停止全部任务".into());
                }
            }

            Cmd::Logout => {
                if let Some(e) = &engine {
                    e.stop_all();
                }
                if let Some(c) = client.as_ref() {
                    // Also kills the session server-side, so the key cannot be reused.
                    let _ = c.sign_out().await;
                }
                if let Some(s) = &session {
                    s.set_readonly();
                }
                client = None;
                login_token = None;
                password_token = None;
                session = None;
                engine = None;
                let _ = std::fs::remove_file(&session_path);
                let _ = std::fs::remove_file(format!("{}.tmp", session_path.to_string_lossy()));
                log("已退出登录，本地凭证已删除".into());
                let _ = ev.send(Ev::LoggedOut);
            }
        }
    }
}

async fn display_user(c: &Client) -> String {
    match c.get_me().await {
        Ok(u) => {
            let name = u.first_name().unwrap_or("").trim().to_string();
            if name.is_empty() {
                format!("id {}", u.id())
            } else {
                name
            }
        }
        Err(_) => "已登录".to_string(),
    }
}

// ---------------------------------------------------------------- target parsing

struct Target {
    channel: String,
    post: Option<i32>,
}

fn parse_target(input: &str) -> Result<Target, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("链接为空".into());
    }
    let lower = t.to_ascii_lowercase();

    // tg://resolve?domain=name&post=123
    if lower.starts_with("tg://resolve") {
        let q = t.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut domain = None;
        let mut post = None;
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "domain" => domain = Some(v.to_string()),
                    "post" => post = v.parse::<i32>().ok(),
                    _ => {}
                }
            }
        }
        return domain
            .map(|d| Target { channel: d, post })
            .ok_or_else(|| "tg:// 链接里缺少 domain".to_string());
    }

    let raw = if let Some(p) = lower.find("t.me/") {
        &t[p + 5..]
    } else if let Some(p) = lower.find("telegram.me/") {
        &t[p + 12..]
    } else {
        t.trim_start_matches('@')
    };

    let path = raw.split('?').next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let parts: Vec<&str> = if parts.first() == Some(&"s") {
        parts[1..].to_vec()
    } else {
        parts
    };

    if parts.is_empty() {
        return Err("链接里没有频道名".into());
    }
    if parts[0] == "c" {
        return Err("这是私有频道链接（/c/…），需要 access_hash；请改用 @用户名 或公开频道链接".into());
    }
    if parts[0] == "joinchat" || parts[0].starts_with('+') {
        return Err("邀请链接暂不支持：请先用账号加入该频道，再填 @用户名".into());
    }

    let channel = parts[0].to_string();
    if parts.len() >= 2 {
        if let Ok(id) = parts[1].parse::<i32>() {
            return Ok(Target {
                channel,
                post: Some(id),
            });
        }
    }
    Ok(Target {
        channel,
        post: None,
    })
}

#[derive(Clone)]
struct Item {
    media: Media,
    name: String,
    size: u64,
    /// Post date, pre-formatted in the local time zone.
    date: String,
    caption: String,
    link: String,
    /// Album id: files posted together share one caption, so the ones without
    /// text can borrow the title of the one that has it.
    grouped: Option<i64>,
    /// True when `name` fell through to `channel_id` — nothing readable was
    /// found, so the album title may still improve it.
    fallback: bool,
}

async fn collect_videos(
    client: &Client,
    target: &Target,
    limit: usize,
    keyword: &str,
    ev: &mpsc::Sender<Ev>,
) -> Result<Vec<Item>, String> {
    let username = target.channel.as_str();
    let peer = client
        .resolve_username(username)
        .await
        .map_err(|e| format!("解析频道 @{username} 失败: {e}"))?
        .ok_or_else(|| format!("找不到频道 @{username}（用户名拼写错误，或账号加入了敏感内容过滤）"))?;
    let peer = peer
        .to_ref()
        .await
        .map_err(|e| format!("获取频道引用失败: {e}"))?
        .ok_or_else(|| format!("@{username} 没有可用的访问引用"))?;

    let keyword = keyword.trim();
    let mut items = Vec::new();
    if let Some(id) = target.post {
        let msgs = client
            .get_messages_by_id(peer, &[id])
            .await
            .map_err(|e| format!("读取帖子 {id} 失败: {e}"))?;
        for m in msgs.into_iter().flatten() {
            if let Some(item) = item_from(&m, &target.channel) {
                items.push(item);
            }
        }
        if items.is_empty() {
            return Err(format!(
                "帖子 {id} 里没有视频（可能是图片/纯文本、已删除，或该账号看不到这条消息）"
            ));
        }
    } else if !keyword.is_empty() {
        // Telegram matches the text server-side, so one search reaches back
        // through the whole channel — no need to paginate through history.
        let mut iter = client.search_messages(peer).query(keyword);
        // A keyword can match plenty of text-only posts; bound the walk so a
        // channel that never matches does not spin forever.
        let cap = limit.saturating_mul(20).max(100);
        let mut seen = 0usize;
        while let Some(m) = iter
            .next()
            .await
            .map_err(|e| format!("在 @{username} 内搜索「{keyword}」失败: {e}"))?
        {
            seen += 1;
            if let Some(item) = item_from(&m, &target.channel) {
                items.push(item);
            }
            if items.len() >= limit || seen >= cap {
                break;
            }
        }
        let _ = ev.send(Ev::Log(format!(
            "@{username} 搜索「{keyword}」：扫了 {seen} 条结果，命中 {} 个视频",
            items.len()
        )));
        if items.is_empty() {
            return Err(format!("@{username} 里没有匹配「{keyword}」的视频"));
        }
    } else {
        let mut iter = client.iter_messages(peer);
        let mut seen = 0usize;
        while let Some(m) = iter
            .next()
            .await
            .map_err(|e| format!("遍历 @{username} 的消息失败: {e}"))?
        {
            seen += 1;
            if let Some(item) = item_from(&m, &target.channel) {
                items.push(item);
            }
            if seen >= limit || items.len() >= limit {
                break;
            }
        }
        let _ = ev.send(Ev::Log(format!(
            "扫描最近 {seen} 条消息，命中 {} 个视频",
            items.len()
        )));
        if items.is_empty() {
            return Err(format!("最近 {seen} 条消息里没有视频"));
        }
    }
    // Album members were collected without a caption; give them the group's
    // title now that the whole set is known.
    fix_album_names(&mut items);
    if diag() {
        for it in &items {
            file_log(&format!("[命名] {} <- {}", it.name, it.link));
        }
    }
    Ok(items)
}

fn item_from(msg: &Message, channel: &str) -> Option<Item> {
    let media = msg.media()?;
    let id = msg.id();
    let (name, size, raw_file_name, fallback) = match &media {
        Media::Document(d) => {
            let mime = d.mime_type().unwrap_or("");
            let fname = d.name();
            let looks_video =
                mime.starts_with("video/") || fname.map(has_video_ext).unwrap_or(false);
            if !looks_video {
                return None;
            }
            // Telegram only carries a file name when the uploader kept one.
            // Phone uploads arrive as `VID_20240101_123456.mp4`, and channel
            // posts often have none at all — so the caption underneath the
            // video is the better source in practice.
            let from_file = fname.map(sanitize_name).filter(|s| {
                let stem = strip_ext(s);
                // `9月21日.mp4` is a readable name but says nothing about the
                // episode; the caption below it usually still does.
                is_readable_stem(stem) && !looks_like_date(stem)
            });
            let title = from_file.or_else(|| caption_title(msg.text()));
            let fallback = title.is_none();
            let mut name = title.unwrap_or_else(|| format!("{channel}_{id}"));
            if !has_video_ext(&name) {
                name.push_str(".mp4");
            }
            (
                name,
                d.size().unwrap_or(0) as u64,
                fname.unwrap_or("").to_string(),
                fallback,
            )
        }
        _ => return None,
    };
    let link = format!("https://t.me/{channel}/{id}");
    if diag() {
        // `TGVD_DIAG=1` records what Telegram actually sent, so a bad file name
        // can be traced back to "no name" vs "a name we rejected".
        file_log(&format!(
            "[原始] {link} | 文件名={raw_file_name:?} | 分组={:?} | 配文={:?}",
            msg.grouped_id(),
            msg.text().chars().take(120).collect::<String>()
        ));
    }
    Some(Item {
        media,
        name,
        size,
        date: format_ts(msg.date()),
        caption: caption_preview(msg.text()),
        link,
        grouped: msg.grouped_id(),
        fallback,
    })
}

/// `TGVD_DIAG=1` turns on verbose per-message logging.
fn diag() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TGVD_DIAG").is_some())
}

/// Post date in the user's own time zone, e.g. `2026-09-21 20:15`.
fn format_ts(ts: jiff::Timestamp) -> String {
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    format!("{}", zoned.strftime("%Y-%m-%d %H:%M"))
}

/// Caption flattened to one line, for the search result list.
fn caption_preview(text: &str) -> String {
    let mut out = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
        if out.chars().count() > 90 {
            break;
        }
    }
    out.chars().take(90).collect()
}

// ---------------------------------------------------------------- download engine

/// `upload.getFile` caps a single request at 512 KiB, so one request == one chunk.
const CHUNK: u64 = 512 * 1024;
/// Upper bound on parallel chunks per file.
///
/// `iter_download` sends one chunk and waits for the reply before sending the
/// next, so a single stream moves at `CHUNK / round-trip`. Measured round-trip
/// to the DC is ~85 ms, which caps one stream at roughly 6 MB/s — the only way
/// to go faster is to have several chunks in flight.
const MAX_SEGMENTS: usize = 32;
/// How often progress numbers are pushed to the UI and the resume file refreshed.
const TICK: Duration = Duration::from_millis(300);

struct DlCfg {
    out_dir: PathBuf,
    overwrite: bool,
    segments: usize,
    /// Empty means "walk recent messages"; otherwise search inside the channel.
    keyword: String,
}

/// Live counters of one task, shared between its segment workers and the reporter.
struct Shared {
    stop: AtomicBool,
    /// Segments still transferring, i.e. the connection count shown in the UI.
    conns: AtomicU32,
    /// Bytes finished per segment (always a whole number of chunks).
    segs: Vec<AtomicU64>,
}

impl Shared {
    fn total(&self) -> u64 {
        self.segs.iter().map(|a| a.load(Ordering::Relaxed)).sum()
    }

    fn snapshot(&self) -> Vec<u64> {
        self.segs
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect()
    }
}

struct Spec {
    id: u64,
    link: String,
    name: String,
    size: u64,
    media: Media,
    dest: PathBuf,
    part: PathBuf,
    state: PathBuf,
    overwrite: bool,
    /// `(first chunk, chunk count)` per parallel connection; count 0 = read to EOF.
    plan: Vec<(u64, u64)>,
    shared: Arc<Shared>,
}

struct Registry {
    next_id: u64,
    specs: HashMap<u64, Arc<Spec>>,
    /// File names already handed out, so two posts with the same title do not
    /// overwrite each other.
    names: HashSet<String>,
}

/// Owns the queue: link resolution, task ids, the concurrency limit and the
/// per-task `stop` flags. Cheap to clone through `Arc`.
struct Engine {
    client: Client,
    ev: mpsc::Sender<Ev>,
    sem: Mutex<Arc<Semaphore>>,
    permits: AtomicU32,
    reg: Mutex<Registry>,
}

impl Engine {
    fn new(client: Client, ev: mpsc::Sender<Ev>, files: usize) -> Self {
        let files = files.clamp(1, 8) as u32;
        Self {
            client,
            ev,
            sem: Mutex::new(Arc::new(Semaphore::new(files as usize))),
            permits: AtomicU32::new(files),
            reg: Mutex::new(Registry {
                next_id: 1,
                specs: HashMap::new(),
                names: HashSet::new(),
            }),
        }
    }

    /// Changing "同时下载几个" only affects tasks queued from now on; files that
    /// are already running keep their permit and finish normally.
    fn set_files(&self, files: usize) {
        let want = files.clamp(1, 8) as u32;
        if self.permits.swap(want, Ordering::Relaxed) == want {
            return;
        }
        if let Ok(mut g) = self.sem.lock() {
            *g = Arc::new(Semaphore::new(want as usize));
        }
    }

    fn spec(&self, id: u64) -> Option<Arc<Spec>> {
        self.reg.lock().ok()?.specs.get(&id).cloned()
    }

    fn stop(&self, id: u64) {
        if let Ok(r) = self.reg.lock() {
            if let Some(sp) = r.specs.get(&id) {
                sp.shared.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    fn stop_all(&self) {
        if let Ok(r) = self.reg.lock() {
            for sp in r.specs.values() {
                sp.shared.stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Resolves links and turns every video found into its own task + download.
async fn resolve_and_queue(eng: Arc<Engine>, links: Vec<String>, limit: usize, cfg: DlCfg) {
    let ev = eng.ev.clone();
    if let Err(e) = std::fs::create_dir_all(&cfg.out_dir) {
        let _ = ev.send(Ev::Log(format!(
            "创建输出目录 {} 失败: {e}",
            cfg.out_dir.display()
        )));
    }
    for raw in links {
        let raw = raw.trim().to_string();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let target = match parse_target(&raw) {
            Ok(t) => t,
            Err(e) => {
                let _ = ev.send(Ev::Log(format!("{raw}: {e}")));
                continue;
            }
        };
        let _ = ev.send(Ev::Stage(format!("正在读取 @{}…", target.channel)));
        match collect_videos(&eng.client, &target, limit, &cfg.keyword, &ev).await {
            Ok(list) => {
                let found = list.len();
                for item in list {
                    enqueue_item(&eng, item, &cfg);
                }
                let _ = ev.send(Ev::Log(format!("@{}: 找到 {found} 个视频", target.channel)));
            }
            Err(e) => {
                let _ = ev.send(Ev::Log(format!("@{}: {e}", target.channel)));
            }
        }
    }
    let _ = ev.send(Ev::Stage("空闲".into()));
}

/// Registers one video as a task and starts (or queues) its transfer.
///
/// Shared by the "paste a link and go" path and by "download the ticked search
/// results", so both behave identically.
fn enqueue_item(eng: &Arc<Engine>, item: Item, cfg: &DlCfg) {
    let plan = plan_segments(item.size, cfg.segments);
    let (id, name) = {
        let Ok(mut r) = eng.reg.lock() else {
            return;
        };
        let name = unique_name(&item.name, &mut r.names);
        let id = r.next_id;
        r.next_id += 1;
        (id, name)
    };
    let dest = cfg.out_dir.join(&name);
    let spec = Arc::new(Spec {
        id,
        link: item.link,
        name: name.clone(),
        size: item.size,
        media: item.media,
        part: part_path(&dest),
        state: state_path(&dest),
        dest,
        overwrite: cfg.overwrite,
        shared: Arc::new(Shared {
            stop: AtomicBool::new(false),
            conns: AtomicU32::new(0),
            segs: plan.iter().map(|_| AtomicU64::new(0)).collect(),
        }),
        plan,
    });
    if let Ok(mut r) = eng.reg.lock() {
        r.specs.insert(id, spec.clone());
    }
    let _ = eng.ev.send(Ev::Added(TaskInfo {
        id,
        name: spec.name.clone(),
        size: spec.size,
        link: spec.link.clone(),
        path: spec.dest.to_string_lossy().to_string(),
    }));
    queue_file(eng.clone(), spec);
}

/// Waits for a free slot and then downloads. Several of these run at the same
/// time, which is what makes multi-task downloading work.
fn queue_file(eng: Arc<Engine>, spec: Arc<Spec>) {
    tokio::spawn(async move {
        let sem = match eng.sem.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let Ok(_permit) = sem.acquire_owned().await else {
            return;
        };
        download_file(eng.clone(), spec).await;
    });
}

async fn download_file(eng: Arc<Engine>, spec: Arc<Spec>) {
    let ev = eng.ev.clone();
    let sh = spec.shared.clone();
    // A stop may have been requested while this task was still queued: honour it
    // instead of clearing the flag and starting the transfer anyway.
    if sh.stop.swap(false, Ordering::Relaxed) {
        let _ = ev.send(Ev::Done {
            id: spec.id,
            status: "已停止".into(),
        });
        return;
    }
    sh.conns.store(0, Ordering::Relaxed);

    if spec.dest.exists() && !spec.overwrite {
        let n = std::fs::metadata(&spec.dest)
            .map(|m| m.len())
            .unwrap_or(spec.size);
        let _ = ev.send(Ev::Progress {
            id: spec.id,
            done: n,
            speed: 0.0,
            conns: 0,
        });
        let _ = ev.send(Ev::Done {
            id: spec.id,
            status: "已存在".into(),
        });
        return;
    }
    if spec.overwrite {
        let _ = std::fs::remove_file(&spec.dest);
    }

    // Only trust a resume file written for exactly this size; anything else is
    // a leftover from a different version of the file.
    let mut resume = vec![0u64; sh.segs.len()];
    if !spec.overwrite {
        if let Ok(bytes) = std::fs::read(&spec.state) {
            if let Ok(st) = serde_json::from_slice::<ResumeState>(&bytes) {
                if st.size == spec.size && st.segs.len() == resume.len() {
                    resume = st.segs;
                }
            }
        }
    }
    for (i, v) in resume.iter().enumerate() {
        sh.segs[i].store(*v, Ordering::Relaxed);
    }
    let _ = ev.send(Ev::Status {
        id: spec.id,
        status: "下载中".into(),
    });

    let mut set = JoinSet::new();
    for (idx, (start, count)) in spec.plan.iter().enumerate() {
        let e = eng.clone();
        let sp = spec.clone();
        let (start, count) = (*start, *count);
        sh.conns.fetch_add(1, Ordering::Relaxed);
        set.spawn(async move { run_segment(&e, &sp, idx, start, count).await });
    }

    let mut stopped = false;
    let mut err: Option<String> = None;
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_done = sh.total();
    let mut last_t = Instant::now();
    let mut speed = 0.0f64;
    let mut saved_at = Instant::now();

    loop {
        tokio::select! {
            joined = set.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(e))) => {
                    err = Some(e);
                    set.abort_all();
                    break;
                }
                Some(Err(_)) => {
                    set.abort_all();
                    break;
                }
            },
            _ = ticker.tick() => {
                let done = sh.total();
                let dt = last_t.elapsed().as_secs_f64();
                if dt > 0.05 {
                    let inst = done.saturating_sub(last_done) as f64 / dt;
                    speed = if speed == 0.0 { inst } else { speed * 0.6 + inst * 0.4 };
                    last_done = done;
                    last_t = Instant::now();
                }
                let _ = ev.send(Ev::Progress {
                    id: spec.id,
                    done,
                    speed,
                    conns: sh.conns.load(Ordering::Relaxed),
                });
                if saved_at.elapsed() >= Duration::from_secs(2) {
                    save_state(&spec);
                    saved_at = Instant::now();
                }
                if sh.stop.swap(false, Ordering::Relaxed) {
                    stopped = true;
                    set.abort_all();
                    break;
                }
            }
        }
    }
    // Dropping the set cancels whatever is still running.
    drop(set);

    let done = sh.total();
    if stopped {
        save_state(&spec);
        let _ = ev.send(Ev::Progress {
            id: spec.id,
            done,
            speed: 0.0,
            conns: 0,
        });
        let _ = ev.send(Ev::Done {
            id: spec.id,
            status: "已停止".into(),
        });
        return;
    }
    if let Some(e) = err {
        save_state(&spec);
        let _ = ev.send(Ev::Done {
            id: spec.id,
            status: format!("错误: {e}"),
        });
        return;
    }
    if spec.size == 0 || done >= spec.size {
        match tokio::fs::rename(&spec.part, &spec.dest).await {
            Ok(()) => {
                let _ = std::fs::remove_file(&spec.state);
                let _ = ev.send(Ev::Progress {
                    id: spec.id,
                    done: done.max(spec.size),
                    speed: 0.0,
                    conns: 0,
                });
                let _ = ev.send(Ev::Done {
                    id: spec.id,
                    status: "完成".into(),
                });
            }
            Err(e) => {
                let _ = ev.send(Ev::Done {
                    id: spec.id,
                    status: format!("改名失败: {e}"),
                });
            }
        }
    } else {
        save_state(&spec);
        let _ = ev.send(Ev::Done {
            id: spec.id,
            status: format!("未完成（{}）", crate::format_bytes(done)),
        });
    }
}

/// One connection: streams its own chunk range straight into the `.part` file.
/// Ranges are disjoint and chunk aligned, so several of these can write at once.
async fn run_segment(
    eng: &Engine,
    spec: &Spec,
    idx: usize,
    start_chunk: u64,
    chunk_count: u64,
) -> Result<(), String> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let sh = &spec.shared;
    let have = sh.segs[idx].load(Ordering::Relaxed);
    let done_chunks = have / CHUNK;
    if chunk_count != 0 && done_chunks >= chunk_count {
        sh.conns.fetch_sub(1, Ordering::Relaxed);
        return Ok(());
    }
    let begin = start_chunk + done_chunks;
    let mut want = if chunk_count == 0 {
        u64::MAX
    } else {
        (chunk_count - done_chunks) * CHUNK
    };

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&spec.part)
        .await
        .map_err(|e| format!("打开文件失败: {e}"))?;
    file.seek(std::io::SeekFrom::Start(begin * CHUNK))
        .await
        .map_err(|e| format!("定位文件失败: {e}"))?;

    let mut iter = eng
        .client
        .iter_download(&spec.media)
        .chunk_size(CHUNK as i32)
        .skip_chunks(begin as i32);

    let mut written = have;
    while !sh.stop.load(Ordering::Relaxed) {
        if want == 0 {
            break;
        }
        match iter.next().await {
            Ok(Some(buf)) => {
                let take = (buf.len() as u64).min(want) as usize;
                file.write_all(&buf[..take])
                    .await
                    .map_err(|e| format!("写入失败: {e}"))?;
                written += take as u64;
                want = want.saturating_sub(take as u64);
                sh.segs[idx].store(written, Ordering::Relaxed);
            }
            Ok(None) => break,
            Err(e) => {
                let _ = file.flush().await;
                sh.conns.fetch_sub(1, Ordering::Relaxed);
                return Err(format!("下载中断: {e}"));
            }
        }
    }
    let _ = file.flush().await;
    sh.conns.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

/// Splits a file into chunk-aligned ranges, one per connection.
fn plan_segments(size: u64, segments: usize) -> Vec<(u64, u64)> {
    if size == 0 {
        // Unknown length: a single stream that ends at EOF.
        return vec![(0, 0)];
    }
    let total = size.div_ceil(CHUNK);
    let n = (segments.clamp(1, MAX_SEGMENTS) as u64).min(total).max(1);
    let base = total / n;
    let rem = total % n;
    let mut out = Vec::with_capacity(n as usize);
    let mut start = 0u64;
    for i in 0..n {
        let len = base + u64::from(i < rem);
        out.push((start, len));
        start += len;
    }
    out
}

/// `movie.mp4` -> `movie.mp4 (2).mp4` when the name is already taken.
fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i..]),
        _ => (base, ""),
    };
    for n in 2..1000 {
        let cand = format!("{stem} ({n}){ext}");
        if used.insert(cand.clone()) {
            return cand;
        }
    }
    base.to_string()
}

/// `<name>` -> `<name>.part`: the file being written, so a half-finished
/// transfer can never be mistaken for a finished video.
fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "video.mp4".into());
    dest.with_file_name(format!("{name}.part"))
}

/// Resume bookkeeping next to the `.part` file: bytes done per segment.
fn state_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "video.mp4".into());
    dest.with_file_name(format!("{name}.part.tgvd"))
}

#[derive(Serialize, Deserialize)]
struct ResumeState {
    size: u64,
    segs: Vec<u64>,
}

fn save_state(spec: &Spec) {
    let st = ResumeState {
        size: spec.size,
        segs: spec.shared.snapshot(),
    };
    let Ok(bytes) = serde_json::to_vec(&st) else {
        return;
    };
    let tmp = PathBuf::from(format!("{}.tmp", spec.state.to_string_lossy()));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &spec.state);
    }
}

fn has_video_ext(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".mp4", ".m4v", ".mkv", ".mov", ".webm", ".avi", ".ts", ".flv"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// `movie.mp4` -> `movie`. Used before judging whether a file name is useful.
fn strip_ext(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Whether a name actually tells you anything about the video.
///
/// `vid.mp4` is what apps name their uploads; typing that into a download
/// manager is worse than falling back to the post caption.
fn is_readable_stem(stem: &str) -> bool {
    let s = stem.trim();
    if s.chars().count() < 4 {
        return false;
    }
    let separator = |c: char| c.is_ascii_digit() || c == '_' || c == '-' || c == ' ';
    if s.chars().all(separator) {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    for prefix in ["vid", "img", "video", "mov", "trim", "record", "screen", "photo"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let rest = rest.trim_start_matches(|c: char| c == '_' || c == '-' || c == ' ');
            if !rest.is_empty() && rest.chars().all(separator) {
                return false;
            }
        }
    }
    true
}

/// First meaningful line of the post caption, used when there is no usable
/// file name. Channel posts look like `EP01 #剧名 @channel https://t.me/…`, so
/// hashtags, mentions and links are dropped and the rest is truncated.
fn caption_title(text: &str) -> Option<String> {
    const MAX_CHARS: usize = 60;
    // The show name usually lives only in the trailing hashtags, so it is
    // pulled out first and prepended to the first readable line.
    let show = show_name(text);
    for raw in text.lines() {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            if word.starts_with('#')
                || word.starts_with('@')
                || word.starts_with("http://")
                || word.starts_with("https://")
                || word.starts_with("www.")
            {
                continue;
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        let line = strip_symbols(&line);
        let line = sanitize_name(&line);
        if line.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
            continue;
        }
        // Daily channels often open the post with just the air date
        // ("9月21日"). That is not a title — keep looking down the caption.
        if looks_like_date(&line) {
            continue;
        }
        let line: String = line.chars().take(MAX_CHARS).collect();
        return Some(match &show {
            Some(show) if !line.contains(show.as_str()) => format!("{show} {line}"),
            _ => line,
        });
    }
    // A caption made of nothing but hashtags still names the show.
    show
}

/// Picks the show name out of the hashtags: `#《琼明神女录 》` -> `琼明神女录`.
///
/// A hashtag wrapped in 《》 is the show name; otherwise the first plausible tag
/// is used, skipping the generic ones channels append to every post.
fn show_name(text: &str) -> Option<String> {
    let mut fallback: Option<String> = None;
    for line in text.lines() {
        let head = line.trim_start();
        // The trailing "关键词:" line repeats the tags; it is the last resort.
        let tag_list = head.starts_with("关键词") || head.starts_with("關鍵詞");
        for tok in head.split_whitespace() {
            let Some(rest) = tok.strip_prefix('#') else {
                continue;
            };
            let wrapped = rest.starts_with('《');
            let name = strip_symbols(rest);
            if name.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
                continue;
            }
            if wrapped {
                return Some(name);
            }
            if !tag_list && fallback.is_none() && !is_generic_tag(&name) {
                fallback = Some(name);
            }
        }
    }
    fallback
}

/// `短剧` / `AI短剧` / `频道` and friends appear on every post of a channel, so
/// using them as the title would make every file look the same.
fn is_generic_tag(name: &str) -> bool {
    const NOISE: [&str; 8] = ["短剧", "影视", "资源", "频道", "合集", "推荐", "福利", "电影"];
    NOISE.iter().any(|n| name.contains(n))
}

/// Removes emoji and decorative punctuation, which channels use as bullets:
/// `😀😀😀😀 》 EP1 黄果版` -> `EP1 黄果版`.
fn strip_symbols(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || " -_.《》()（）[]【】:：!！?？+&".contains(*c))
        .collect();
    // Book-title brackets belong to the decoration, not the name: `#《剧名》`
    // must come out as `剧名` on both ends.
    kept.trim()
        .trim_start_matches(|c: char| "《》<>()（）[]【】-_.:：,，。!！?？+&".contains(c))
        .trim_end_matches(|c: char| "《》<>()（）[]【】-_.:：,，。、!！?？+&".contains(c))
        .trim()
        .to_string()
}

/// Files posted as one album share a single caption: Telegram attaches it to
/// one message and leaves the others empty. Borrow that title for the rest and
/// number them, so a six-part upload is not six identical `channel_id` names.
fn fix_album_names(items: &mut [Item]) {
    let mut titles: HashMap<i64, String> = HashMap::new();
    for it in items.iter() {
        if let (Some(g), false) = (it.grouped, it.fallback) {
            titles
                .entry(g)
                .or_insert_with(|| strip_ext(&it.name).to_string());
        }
    }
    if titles.is_empty() {
        return;
    }
    let mut seen: HashMap<i64, usize> = HashMap::new();
    for it in items.iter_mut() {
        let Some(g) = it.grouped else { continue };
        let Some(title) = titles.get(&g) else {
            continue;
        };
        let n = {
            let e = seen.entry(g).or_insert(0);
            *e += 1;
            *e
        };
        if it.fallback {
            let ext = it
                .name
                .rsplit_once('.')
                .map(|(_, e)| e.to_string())
                .unwrap_or_else(|| "mp4".to_string());
            it.name = format!("{title}_{n}.{ext}");
            it.fallback = false;
        }
    }
}

/// True when the line carries nothing but a date and/or a time.
///
/// `9月21日`, `2026-09-21`, `21:30`, `周日` — all useless as file names, and all
/// common as the first line of a channel post.
fn looks_like_date(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return false;
    }
    let mut rest = compact.clone();
    let mut words = 0usize;
    for w in [
        "星期", "周", "年", "月", "日", "号", "点", "时", "分", "秒", "凌晨", "上午", "中午",
        "下午", "晚上",
    ] {
        while let Some(pos) = rest.find(w) {
            rest.replace_range(pos..pos + w.len(), "");
            words += 1;
        }
    }
    let has_digit = rest.chars().any(|c| c.is_ascii_digit());
    let only_punct = rest
        .chars()
        .all(|c| c.is_ascii_digit() || "-/.:~·,，、()（）".contains(c));
    if has_digit && only_punct {
        return true;
    }
    // "周日" is entirely date words with no digits left.
    rest.is_empty() && words > 0
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    cleaned.trim().trim_matches('.').to_string()
}

// ---------------------------------------------------------------- session storage

/// A tiny on-disk session store.
///
/// `grammers-session`'s default storage is SQLite (via `libsql`, a bundled C
/// library). Building that on Windows/MinGW is heavy, and we only need to persist
/// a few maps, so this keeps the single-file-exe promise with plain JSON instead.
#[derive(Serialize, Deserialize)]
struct Persist {
    home_dc: i32,
    dc_options: Vec<DcOption>,
    peer_infos: Vec<PeerInfo>,
    updates_state: UpdatesState,
    /// Display name of the logged-in account. Cached so a resume does not need
    /// an extra `users.getFullUser` round trip (~0.5 s on this network) just to
    /// fill in the title of the account panel.
    #[serde(default)]
    me_name: Option<String>,
}

pub struct JsonSession {
    data: Mutex<SessionData>,
    me_name: Mutex<Option<String>>,
    path: PathBuf,
    /// Set after a log-out so a still-running sender cannot recreate the file.
    readonly: AtomicBool,
}

impl JsonSession {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut data = SessionData::default();
        let mut me_name = None;
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(p) = serde_json::from_slice::<Persist>(&bytes) {
                data.home_dc = p.home_dc;
                for opt in p.dc_options {
                    data.dc_options.insert(opt.id, opt);
                }
                for info in p.peer_infos {
                    data.peer_infos.insert(info.id(), info);
                }
                data.updates_state = p.updates_state;
                me_name = p.me_name;
            }
        }
        Self {
            data: Mutex::new(data),
            me_name: Mutex::new(me_name),
            path,
            readonly: AtomicBool::new(false),
        }
    }

    /// Last known display name, if a previous run stored one.
    pub fn cached_name(&self) -> Option<String> {
        self.me_name.lock().ok().and_then(|n| n.clone())
    }

    pub fn set_cached_name(&self, name: &str) {
        if let Ok(mut n) = self.me_name.lock() {
            *n = Some(name.to_string());
        }
    }

    /// Forces a snapshot to disk (used right after a successful sign-in, so the
    /// credentials are safely stored even if the app is closed immediately).
    pub fn flush(&self) {
        let snap = match self.data.lock() {
            Ok(d) => Self::snapshot(&d),
            Err(_) => return,
        };
        self.write_persist(snap);
    }

    pub fn set_readonly(&self) {
        self.readonly.store(true, Ordering::Relaxed);
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SessionData>, SessionErr> {
        self.data.lock().map_err(|_| SessionErr)
    }

    /// Takes a snapshot while holding the lock, so no guard is held across a write.
    fn snapshot(d: &SessionData) -> Persist {
        Persist {
            home_dc: d.home_dc,
            dc_options: d.dc_options.values().cloned().collect(),
            peer_infos: d.peer_infos.values().cloned().collect(),
            updates_state: d.updates_state.clone(),
            me_name: None,
        }
    }

    /// Fills in the cached display name and writes the file. Every caller goes
    /// through here so a plain session update cannot drop that field.
    fn write_persist(&self, mut p: Persist) {
        if let Ok(n) = self.me_name.lock() {
            p.me_name = n.clone();
        }
        self.write(&p);
    }

    fn write(&self, p: &Persist) {
        if self.readonly.load(Ordering::Relaxed) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(p) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let tmp = PathBuf::from(format!("{}.tmp", self.path.to_string_lossy()));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[derive(Debug)]
pub struct SessionErr;

impl fmt::Display for SessionErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session lock poisoned")
    }
}

impl std::error::Error for SessionErr {}

impl Session for JsonSession {
    type Error = SessionErr;

    fn home_dc_id(&self) -> Result<i32, Self::Error> {
        Ok(self.lock()?.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let snap = {
                let mut d = self.lock()?;
                d.home_dc = dc_id;
                Self::snapshot(&d)
            };
            self.write_persist(snap);
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Self::Error> {
        Ok(self.lock()?.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let opt = dc_option.clone();
        Box::pin(async move {
            let snap = {
                let mut d = self.lock()?;
                d.dc_options.insert(opt.id, opt);
                Self::snapshot(&d)
            };
            self.write_persist(snap);
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Self::Error>> {
        Box::pin(async move { Ok(self.lock()?.peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let snap = {
                let mut d = self.lock()?;
                d.peer_infos.insert(peer.id(), peer);
                Self::snapshot(&d)
            };
            self.write_persist(snap);
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Self::Error>> {
        Box::pin(async move { Ok(self.lock()?.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let snap = {
                let mut d = self.lock()?;
                match update {
                    UpdateState::All(state) => d.updates_state = state,
                    UpdateState::Primary { pts, date, seq } => {
                        d.updates_state.pts = pts;
                        d.updates_state.date = date;
                        d.updates_state.seq = seq;
                    }
                    UpdateState::Secondary { qts } => d.updates_state.qts = qts,
                    UpdateState::Channel { id, pts } => {
                        d.updates_state.channels.retain(|c| c.id != id);
                        d.updates_state.channels.push(ChannelState { id, pts });
                    }
                }
                Self::snapshot(&d)
            };
            self.write_persist(snap);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cover_the_whole_file() {
        // 3 MiB = 6 chunks, split over 4 connections -> 2, 2, 1, 1
        assert_eq!(
            plan_segments(3 * 1024 * 1024, 4),
            vec![(0, 2), (2, 2), (4, 1), (5, 1)]
        );
        // More connections than chunks: never an empty range.
        assert_eq!(plan_segments(CHUNK as u64, 8), vec![(0, 1)]);
        // Unknown size: a single stream that ends at EOF.
        assert_eq!(plan_segments(0, 4), vec![(0, 0)]);
    }

    #[test]
    fn duplicate_names_get_a_suffix() {
        let mut used = HashSet::new();
        assert_eq!(unique_name("a.mp4", &mut used), "a.mp4");
        assert_eq!(unique_name("a.mp4", &mut used), "a (2).mp4");
        assert_eq!(unique_name("a.mp4", &mut used), "a (3).mp4");
        assert_eq!(unique_name("noext", &mut used), "noext");
        assert_eq!(unique_name("noext", &mut used), "noext (2)");
    }

    #[test]
    fn parses_share_links() {
        let t = parse_target("https://t.me/xgduanju/2942?t=3").unwrap();
        assert_eq!(t.channel, "xgduanju");
        assert_eq!(t.post, Some(2942));

        let t = parse_target("https://t.me/s/telegram/441").unwrap();
        assert_eq!(t.channel, "telegram");
        assert_eq!(t.post, Some(441));

        let t = parse_target("tg://resolve?domain=xgduanju&post=2942").unwrap();
        assert_eq!(t.channel, "xgduanju");
        assert_eq!(t.post, Some(2942));

        let t = parse_target("@somechannel").unwrap();
        assert_eq!(t.channel, "somechannel");
        assert_eq!(t.post, None);
    }

    #[test]
    fn rejects_private_and_invite_links() {
        assert!(parse_target("https://t.me/c/1234567/89").is_err());
        assert!(parse_target("https://t.me/+AbCdEf").is_err());
    }

    #[test]
    fn session_roundtrip_persists_home_dc() {
        let dir = std::env::temp_dir().join(format!("tgvd-session-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.session");
        let _ = std::fs::remove_file(&path);

        {
            let s = JsonSession::open(&path);
            let mut d = s.lock().unwrap();
            d.home_dc = 4;
            let snap = JsonSession::snapshot(&d);
            drop(d);
            s.write(&snap);
        }

        let s2 = JsonSession::open(&path);
        assert_eq!(s2.home_dc_id().unwrap(), 4);
        assert!(s2.dc_option(4).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("a/b:c*d?.mp4"), "a_b_c_d_.mp4");
        assert!(has_video_ext("EP1.MP4"));
        assert!(!has_video_ext("cover.jpg"));
    }

    #[test]
    fn legacy_session_is_copied_to_the_new_location() {
        let dir = std::env::temp_dir().join(format!("tgvd-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = dir.join("app").join("tgvd.session");
        let dst = dir.join("roaming").join("TGVD").join("tgvd.session");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, br#"{"home_dc":2}"#).unwrap();

        let got = prepare_session_with(&dst, std::slice::from_ref(&old));
        assert_eq!(got, dst);
        assert_eq!(std::fs::read(&dst).unwrap(), std::fs::read(&old).unwrap());

        // A second call must keep the migrated file instead of re-copying.
        std::fs::write(&dst, b"newer").unwrap();
        let again = prepare_session_with(&dst, std::slice::from_ref(&old));
        assert_eq!(std::fs::read(&again).unwrap(), b"newer");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_marker_stops_further_writes() {
        let dir = std::env::temp_dir().join(format!("tgvd-ro-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ro.session");
        let s = JsonSession::open(&path);
        s.flush();
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
        s.set_readonly();
        s.flush();
        assert!(!path.exists(), "readonly session must not write again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
