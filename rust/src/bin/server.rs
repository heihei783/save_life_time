//! 签到系统的 HTTP 层：管理口令登录、账号维护、会话刷新/状态检测，以及三种签到。
//!
//! 两条硬约束贯穿全文，改代码时别破坏：
//! 1. 全局状态锁（`Mutex<Store>`）里**只做内存操作**。落盘要走 [Saver]，它做的是
//!    fsync——占着锁做一次 fsync 就能把 /api/session 这类小请求一起卡到前端超时，
//!    表现出来就是"莫名掉登录态"。
//! 2. 一个账号失败不能拖垮整批。每个账号独立执行、独立超时、独立汇报结果，
//!    单次请求不设硬超时，否则前面的结果会一起丢掉。

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use winsignin::{self as core, AccountInfo, LoginResult, RollcallsResult, SignResult};

/// 同时存活的管理会话 token 上限。登录只追加不发作废：其它标签页/设备上的 token
/// 继续有效，直到被新登录挤出这个窗口（8 次登录之后）。
const MAX_SESSION_TOKENS: usize = 8;
/// 单次签到请求里"错峰"累计等待的软预算（秒）。超过就不再等，直接连发剩余账号：
/// Nginx 默认 proxy_read_timeout 是 60s，错峰设成 10~30s 时第 4 个账号就会被切断。
const STAGGER_BUDGET_SECONDS: u64 = 30;
/// 单批签到里同时跑几个账号。默认 1 = 完全串行，学校侧风控对并发最敏感。
const SIGN_BATCH_SIZE: usize = 1;
/// 二维码签到的并发度。
///
/// 二维码几秒到十几秒就换一张，串行 + 错峰根本签不完（账号越多越明显，
/// 表现为"只成功前几个"）。这类签到必须抢时间，所以放开并发并且**不排队等错峰**。
/// 数字/位置类不赶时间，仍然按 [SIGN_BATCH_SIZE] 串行。
const QR_BATCH_SIZE: usize = 8;
/// 状态检测的并发度：这里只是读接口，可以比签到激进一点。
const CHECK_CONCURRENCY: usize = 6;
/// 会话刷新的并发度：CAS 链比较重，保守取 4。
const REFRESH_CONCURRENCY: usize = 4;
/// 单个签到任务的独立超时（秒）。超时只影响这一个账号，后续账号照常执行。
const SIGN_JOB_TIMEOUT_SECONDS: u64 = 45;
/// 单个状态检测任务的独立超时（秒）。
const CHECK_JOB_TIMEOUT_SECONDS: u64 = 15;
/// 单次状态检测的整体预算（秒）：到点后没回来的账号记"检测超时"。
const CHECK_DEADLINE_SECONDS: u64 = 60;
/// 单个会话刷新任务的独立超时（秒）。
const REFRESH_JOB_TIMEOUT_SECONDS: u64 = 60;
/// 单次会话刷新的整体预算（秒）。
const REFRESH_DEADLINE_SECONDS: u64 = 90;
/// 跨课程一键签到一次最多排多少个 (点名 × 账号) 任务。多门课同时点名时
/// 任务数会相乘增长，这里给个上限，超出部分明确回报 truncated，而不是静默丢弃。
const MAX_ALL_SIGN_JOBS: usize = 40;
/// 跨课程一键签到的整体预算（秒）。超过后未执行的任务记为超时，避免请求被反代整条切断。
const ALL_SIGN_DEADLINE_SECONDS: u64 = 150;

#[derive(Clone)]
struct AppState {
    inner: Arc<Mutex<Store>>,
    saver: Arc<Saver>,
}

impl AppState {
    fn new(store: Store) -> Self {
        Self {
            inner: Arc::new(Mutex::new(store)),
            saver: Arc::new(Saver::default()),
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Store {
    /// 最近一次签发的管理会话 token（保留独立字段是为了兼容旧状态文件）。
    token: String,
    /// 仍然有效的历史 token。多标签页/多设备各持一份，登录时只追加、不作废。
    #[serde(default)]
    tokens: Vec<String>,
    accounts: Vec<Account>,
    #[serde(default)]
    settings: Settings,
}

impl Store {
    /// 请求带的 token 是否仍然有效。
    fn accept_token(&self, presented: &str) -> bool {
        !presented.is_empty()
            && (self.token == presented || self.tokens.iter().any(|t| t == presented))
    }

    /// 签发新会话：旧 token **不**立刻作废，只是被挤进窗口。
    fn issue_token(&mut self) -> String {
        let fresh = new_token();
        if !self.token.is_empty() && !self.tokens.iter().any(|t| *t == self.token) {
            self.tokens.push(self.token.clone());
        }
        if !self.tokens.iter().any(|t| *t == fresh) {
            self.tokens.push(fresh.clone());
        }
        if self.tokens.len() > MAX_SESSION_TOKENS {
            let evicted = self.tokens.len() - MAX_SESSION_TOKENS;
            self.tokens.drain(0..evicted);
        }
        self.token = fresh.clone();
        fresh
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    min_delay_seconds: u64,
    max_delay_seconds: u64,
    auto_number: bool,
    radar_scheme: String,
    probes: Vec<RadarProbe>,
}

impl Default for Settings {
    /// 默认错峰 1~2 秒：既能避开"同一秒连打"，又不会让一批请求超过反代的读超时。
    fn default() -> Self {
        Self {
            min_delay_seconds: 1,
            max_delay_seconds: 2,
            auto_number: false,
            radar_scheme: "trilateration".into(),
            probes: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Account {
    id: String,
    username: String,
    name: String,
    student_id: String,
    cookie: String,
    cas_cookie: String,
    bfp: String,
    device_id: String,
    enabled: bool,
    /// 最近一次探测到的会话状态：true = 学校侧判定会话已失效（需要重新登录）。
    #[serde(default)]
    expired: bool,
    /// 最近一次探测的 Unix 时间戳（秒）；0 表示从未检测过。
    #[serde(default)]
    checked_at: u64,
    /// 最近一次探测的说明：过期原因，或检测失败的原因。
    #[serde(default)]
    status_detail: String,
}

#[derive(Deserialize)]
struct AdminLogin {
    password: String,
}

#[derive(Deserialize)]
struct SettingsInput {
    min_delay_seconds: u64,
    max_delay_seconds: u64,
    auto_number: bool,
    radar_scheme: String,
    probes: Vec<RadarProbe>,
}

#[derive(Deserialize)]
struct SchoolLogin {
    username: String,
    password: String,
    bfp: Option<String>,
    remember_me: Option<bool>,
    cas_cookie: Option<String>,
}

/// 批量扫码签到。`account_ids` 是分批执行的关键：前端每批 3 个账号分别调用，
/// 单次请求耗时可控，失败的那批还能单独重试。
#[derive(Deserialize)]
struct QrReq {
    raw: String,
    #[serde(default)]
    account_ids: Option<Vec<String>>,
}

/// 单账号扫码补签。
#[derive(Deserialize)]
struct QrAccountReq {
    account_id: String,
    raw: String,
}

#[derive(Deserialize)]
struct NumberReq {
    rollcall_id: String,
    code: String,
}

#[derive(Deserialize)]
struct NumberAccountReq {
    account_id: String,
    rollcall_id: String,
    #[serde(default)]
    code: String,
}

#[derive(Deserialize)]
struct RadarReq {
    rollcall_id: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct RadarAccountReq {
    account_id: String,
    rollcall_id: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct EmptyAccountReq {
    account_id: String,
    rollcall_id: String,
}

/// 跨课程一键签到：把当前所有进行中的点名，按各自类型对"能看到它的账号"签到。
///
/// 一次请求可能覆盖多门课、多个账号，所以每类点名的参数都是可选的：
/// 二维码类必须由调用方提供 [qr_raw]（服务端无法凭空得到二维码原文），
/// 位置类优先用 [latitude]/[longitude]，没给就退回设置里的探测点中心。
#[derive(Deserialize)]
struct SignAllReq {
    #[serde(default)]
    qr_raw: Option<String>,
    #[serde(default)]
    latitude: Option<f64>,
    #[serde(default)]
    longitude: Option<f64>,
    /// 只处理这些点名；不传或传空数组表示当前所有进行中的点名。
    #[serde(default)]
    rollcall_ids: Option<Vec<String>>,
    /// 默认 false：二维码与点名不匹配时直接跳过（因为 data 和 rollcallId 是一起编码的，
    /// 拿 A 课的码去签 B 课通常会被上游拒绝）。设为 true 时仍然尝试提交，
    /// 让调用方自己验证上游是否接受——上游认了就成功，不认就是一条明确的失败原因。
    #[serde(default)]
    allow_qr_mismatch: bool,
}

/// 单个点名在"一键签到"里的类型判定。
#[derive(Clone, Copy, PartialEq, Eq)]
enum RollcallKind {
    Number,
    Radar,
    Qr,
}

impl RollcallKind {
    fn as_str(self) -> &'static str {
        match self {
            RollcallKind::Number => "number",
            RollcallKind::Radar => "radar",
            RollcallKind::Qr => "qr",
        }
    }
}

/// 一个待执行的签到任务：(点名, 账号)。课程名/标题在输出时按 rollcall_id 回查，
/// 避免同一场点名在几十个任务里重复存一份。
struct AllSignJob {
    rollcall_id: String,
    kind: RollcallKind,
    account_id: String,
    account_name: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct RadarProbe {
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct RadarTriangulateReq {
    rollcall_id: String,
    probes: Vec<RadarProbe>,
}

fn data_path() -> PathBuf {
    PathBuf::from(env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())).join("state.json")
}

fn load_store() -> Store {
    let p = data_path();
    match fs::read(&p) {
        Ok(bytes) => {
            restrict_state_permissions(&p);
            serde_json::from_slice(&bytes).expect("状态文件损坏，拒绝启动以免覆盖账号数据")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => panic!("读取状态文件失败：{e}"),
    }
}

#[cfg(unix)]
fn restrict_state_permissions(path: &PathBuf) {
    // 收紧权限只是加固，不是启动前提：bind mount、NFS/CIFS、部分卷驱动不允许 chmod，
    // 这里 panic 会让整个服务起不来（表现为容器反复重启、网页完全打不开）。
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        eprintln!("警告：无法收紧 {} 的权限（{e}），请自行确认数据目录访问控制", path.display());
    }
}
#[cfg(not(unix))]
fn restrict_state_permissions(_path: &PathBuf) {}

fn create_private_file(path: &PathBuf) -> Result<fs::File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .map_err(|e| format!("创建状态临时文件失败：{e}"))
}

/// 落盘：写临时文件 → fsync → 原子 rename。**调用方必须确保没有持有状态锁**
/// （由 [Saver] 在阻塞线程里调用）。
fn save_store(s: &Store) -> Result<(), String> {
    let p = data_path();
    let parent = p.parent().ok_or_else(|| "DATA_DIR 必须有父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("创建数据目录失败：{e}"))?;
    let tmp = p.with_file_name(format!("state-{}.tmp", new_token()));
    let bytes = serde_json::to_vec_pretty(s).map_err(|e| format!("序列化状态失败：{e}"))?;
    let mut f = create_private_file(&tmp)?;
    f.write_all(&bytes)
        .map_err(|e| format!("写入状态失败：{e}"))?;
    f.sync_all().map_err(|e| format!("同步状态失败：{e}"))?;
    drop(f);
    fs::rename(&tmp, &p).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("原子替换状态文件失败：{e}")
    })
}

/// 落盘闸门：所有写盘串行执行，并且拒绝用比已写版本更旧的快照覆盖新快照。
///
/// 只改状态不写盘会丢数据，持锁写盘会卡住所有请求，所以两者拆开：临界区里改内存
/// 并取快照，出了临界区再交给这里慢慢 fsync。
#[derive(Default)]
struct Saver {
    issued: AtomicU64,
    written: AtomicU64,
    gate: Mutex<()>,
}

impl Saver {
    fn next_seq(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn write(&self, seq: u64, snapshot: Store) {
        let guard = match self.gate.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if seq <= self.written.load(Ordering::SeqCst) {
            return; // 已经有更新的快照落盘了，别用旧状态覆盖它
        }
        match save_store(&snapshot) {
            Ok(()) => {
                self.written.store(seq, Ordering::SeqCst);
            }
            Err(e) => eprintln!("保存状态失败：{e}"),
        }
        drop(guard);
    }

    /// 有运行时就把 fsync 丢给阻塞线程池，没有运行时（单元测试）就同步写。
    fn dispatch(self: &Arc<Self>, seq: u64, snapshot: Store) {
        let saver = Arc::clone(self);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || saver.write(seq, snapshot));
            }
            Err(_) => self.write(seq, snapshot),
        }
    }
}

fn new_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

/// 每个账号一个 device_id。上游按 deviceId 去重，两个账号撞在一起就会被判
/// `device_used` / `deviceAlreadyInUse`——也就是"前几个成功、后面全失败"。
fn new_device_id() -> String {
    new_token()[..16].to_string()
}

fn unique_device_id(accounts: &[Account]) -> String {
    loop {
        let candidate = new_device_id();
        if !candidate.is_empty() && !accounts.iter().any(|a| a.device_id == candidate) {
            return candidate;
        }
    }
}

fn unique_account_id(accounts: &[Account]) -> String {
    loop {
        let candidate = new_token()[..12].to_string();
        if !accounts.iter().any(|a| a.id == candidate) {
            return candidate;
        }
    }
}

/// 收敛 device_id：空的补一个、和历史记录撞车的重新生成。
fn ensure_unique_device_ids(s: &mut Store) {
    let mut seen: HashSet<String> = HashSet::new();
    for account in s.accounts.iter_mut() {
        let current = account.device_id.trim().to_string();
        if !current.is_empty() && !seen.contains(&current) {
            seen.insert(current.clone());
            account.device_id = current;
            continue;
        }
        let mut fresh = new_device_id();
        while fresh.is_empty() || seen.contains(&fresh) {
            fresh = new_device_id();
        }
        seen.insert(fresh.clone());
        account.device_id = fresh;
    }
}

fn env_required(k: &str) -> String {
    env::var(k).unwrap_or_else(|_| panic!("必须设置环境变量 {k}"))
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let expected = env::var("MAIN_ORIGIN").unwrap_or_else(|_| "https://sign.xiaohundun.cc".into());
    origin.map_or(true, |o| o == expected.as_str())
}

/// 管理会话校验。[Store::accept_token] 允许多个 token 并存，手机上登录一次不会
/// 把电脑上的页面踢下线。
fn auth(headers: &HeaderMap, s: &Store) -> bool {
    if !origin_allowed(headers) {
        return false;
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let from_header = bearer
        .strip_prefix("Bearer ")
        .map(str::trim)
        .unwrap_or_default();
    let from_cookie = cookie
        .split(';')
        .filter_map(|part| part.trim().strip_prefix("session="))
        .next()
        .unwrap_or_default();
    s.accept_token(from_header) || s.accept_token(from_cookie)
}

fn rollcalls_api_token_auth(headers: &HeaderMap) -> bool {
    if !origin_allowed(headers) {
        return false;
    }
    let expected = env::var("WINSIGNIN_API_TOKEN").unwrap_or_default();
    if expected.is_empty() {
        return false;
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    bearer == format!("Bearer {expected}")
}

// ---------------------------------------------------------------------------
// 状态访问：读用 read_store，写用 commit。两者都不把锁带到 IO 之外。
// ---------------------------------------------------------------------------

fn lock_store(st: &AppState) -> MutexGuard<'_, Store> {
    match st.inner.lock() {
        Ok(guard) => guard,
        // 别的 handler panic 过也别让后续请求全部 500（那看起来也像掉登录态）
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn read_store<T>(st: &AppState, f: impl FnOnce(&Store) -> T) -> T {
    f(&lock_store(st))
}

/// 在临界区里改状态并取快照，**出临界区**再落盘。
fn commit<T>(st: &AppState, mutate: impl FnOnce(&mut Store) -> (bool, T)) -> T {
    let mut snapshot = None;
    let mut seq = 0;
    let out = {
        let mut s = lock_store(st);
        let (changed, out) = mutate(&mut s);
        if changed {
            seq = st.saver.next_seq();
            snapshot = Some(s.clone());
        }
        out
    };
    if let Some(snapshot) = snapshot {
        st.saver.dispatch(seq, snapshot);
    }
    out
}

fn authorized(st: &AppState, headers: &HeaderMap) -> bool {
    read_store(st, |s| auth(headers, s))
}

fn authorized_accounts(st: &AppState, headers: &HeaderMap) -> Result<Vec<Account>, Response> {
    read_store(st, |s| {
        if auth(headers, s) {
            Ok(s.accounts.clone())
        } else {
            Err(unauthorized())
        }
    })
}

fn authorized_rollcall_accounts(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<Vec<Account>, Response> {
    read_store(st, |s| {
        if auth(headers, s) || rollcalls_api_token_auth(headers) {
            Ok(s.accounts.clone())
        } else {
            Err(unauthorized())
        }
    })
}

fn sign_accounts(
    st: &AppState,
    headers: &HeaderMap,
    account_ids: Option<&[String]>,
) -> Result<(Vec<Account>, Vec<(String, String)>, Settings), Response> {
    read_store(st, |s| {
        if !auth(headers, s) {
            return Err(unauthorized());
        }
        let (picked, missing) = select_accounts(&s.accounts, account_ids);
        Ok((picked, missing, s.settings.clone()))
    })
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"ok":false,"message":"未登录"})),
    )
        .into_response()
}

fn json_result(ok: bool, message: impl Into<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok":ok,"message":message.into()}))
}

fn stagger_seconds(settings: &Settings, index: usize) -> u64 {
    if index == 0 || settings.max_delay_seconds == 0 {
        0
    } else {
        rand::thread_rng().gen_range(settings.min_delay_seconds..=settings.max_delay_seconds)
    }
}

/// 错峰计划，带一个累计软预算：等够 [STAGGER_BUDGET_SECONDS] 之后剩下的账号不再
/// 等，直接连发。这样一批 3~6 个账号的耗时是有上界的，不会因为错峰设置太保守
/// 而被反代的读超时整条切断。
fn stagger_plan(settings: &Settings, count: usize) -> Vec<Duration> {
    let mut used = 0u64;
    (0..count)
        .map(|index| {
            let seconds = stagger_seconds(settings, index);
            if used + seconds > STAGGER_BUDGET_SECONDS {
                0
            } else {
                used += seconds;
                seconds
            }
        })
        .map(Duration::from_secs)
        .collect()
}

/// 以 `limit` 的并发度跑完所有阻塞任务，结果与输入同序。
///
/// - 用窗口而不是一次性全起：签到接口默认 `limit = 1`（完全串行），学校侧风控
///   对并发最敏感。
/// - 每个任务有独立超时，一个账号卡住只影响它自己，后续账号照常执行。
/// - `deadline` 给非签到类接口兜底；签到接口传 None（不能因为整体超时把结果丢掉）。
async fn run_batched<T, F>(
    limit: usize,
    per_job: Duration,
    deadline: Option<tokio::time::Instant>,
    delays: Vec<Duration>,
    jobs: Vec<F>,
) -> Vec<Option<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let limit = limit.max(1);
    let mut results: Vec<Option<T>> = (0..jobs.len()).map(|_| None).collect();
    let mut pending: VecDeque<(usize, tokio::task::JoinHandle<T>)> = VecDeque::new();
    for (index, job) in jobs.into_iter().enumerate() {
        if let Some(delay) = delays.get(index).copied() {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        if pending.len() >= limit {
            if let Some((slot, handle)) = pending.pop_front() {
                results[slot] = await_job(handle, per_job, deadline).await;
            }
        }
        pending.push_back((index, tokio::task::spawn_blocking(job)));
    }
    while let Some((slot, handle)) = pending.pop_front() {
        results[slot] = await_job(handle, per_job, deadline).await;
    }
    results
}

async fn await_job<T>(
    handle: tokio::task::JoinHandle<T>,
    per_job: Duration,
    deadline: Option<tokio::time::Instant>,
) -> Option<T> {
    let job_deadline = tokio::time::Instant::now() + per_job;
    let cut_at = match deadline {
        Some(deadline) => deadline.min(job_deadline),
        None => job_deadline,
    };
    match tokio::time::timeout_at(cut_at, handle).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            eprintln!("后台任务失败：{e}");
            None
        }
        Err(_) => None,
    }
}

/// spawn_blocking 的包装：任务 panic 时不炸掉整个请求。
async fn run_blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    match tokio::task::spawn_blocking(job).await {
        Ok(value) => Some(value),
        Err(e) => {
            eprintln!("阻塞任务失败：{e}");
            None
        }
    }
}

const SESSION_EXPIRED_HINT: &str = "登录已过期";
const CHECK_TIMEOUT_HINT: &str = "检测超时，请稍后重试";
const SIGN_TIMEOUT_HINT: &str = "签到请求超时，未拿到结果，请单独重试该账号";
const MISSING_NUMBER_CODE: &str = "没有签到码：请填写签到码，或在设置里开启自动提取";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_session_expired(message: &str) -> bool {
    message.contains(SESSION_EXPIRED_HINT)
}

/// 一次探测得到的会话结论，只用于更新缓存的账号状态。
enum SessionProbe {
    Ok,
    Expired(String),
    Failed(String),
}

fn probe_from_account(info: AccountInfo) -> SessionProbe {
    if info.ok {
        SessionProbe::Ok
    } else if is_session_expired(&info.message) {
        SessionProbe::Expired(info.message)
    } else {
        SessionProbe::Failed(info.message)
    }
}

fn status_ok(a: &mut Account) {
    a.checked_at = unix_now();
    a.expired = false;
    a.status_detail.clear();
}

fn status_expired(a: &mut Account, message: &str) {
    a.checked_at = unix_now();
    a.expired = true;
    a.status_detail = message.to_string();
}

fn status_check_failed(a: &mut Account, message: &str) {
    a.checked_at = unix_now();
    a.status_detail = format!("状态检测失败：{message}");
}

fn apply_probe(a: &mut Account, probe: &SessionProbe) {
    match probe {
        SessionProbe::Ok => status_ok(a),
        SessionProbe::Expired(message) => status_expired(a, message),
        SessionProbe::Failed(message) => status_check_failed(a, message),
    }
}

fn probe_state(probe: &SessionProbe) -> &'static str {
    match probe {
        SessionProbe::Ok => "ok",
        SessionProbe::Expired(_) => "expired",
        SessionProbe::Failed(_) => "failed",
    }
}

fn probe_message(probe: &SessionProbe) -> String {
    match probe {
        SessionProbe::Ok => "账号状态正常".to_string(),
        SessionProbe::Expired(message) => message.clone(),
        SessionProbe::Failed(message) => format!("状态检测失败：{message}"),
    }
}

/// 把一批探测结论写回账号；只有状态真的变了才落盘，避免轮询把状态文件写个不停。
fn apply_probes(st: &AppState, probes: &[(String, SessionProbe)]) {
    if probes.is_empty() {
        return;
    }
    commit(st, |s| {
        let mut changed = false;
        for (id, probe) in probes {
            let Some(a) = s.accounts.iter_mut().find(|a| a.id == *id) else {
                continue;
            };
            let before = (a.expired, a.status_detail.clone());
            apply_probe(a, probe);
            if (a.expired, a.status_detail.clone()) != before {
                changed = true;
            }
        }
        (changed, ())
    });
}

fn account_json(a: &Account) -> serde_json::Value {
    serde_json::json!({
        "id":a.id,"username":a.username,"name":a.name,"student_id":a.student_id,
        "enabled":a.enabled,"expired":a.expired,"checked_at":a.checked_at,
        "status_detail":a.status_detail,"can_refresh":!a.cas_cookie.trim().is_empty(),
    })
}

fn accounts_json(s: &Store) -> Vec<serde_json::Value> {
    s.accounts.iter().map(account_json).collect()
}

/// 巡检/刷新接口统一的逐账号结论。
#[derive(Serialize)]
struct AccountOutcome {
    account_id: String,
    name: String,
    /// ok / expired / failed / skipped
    state: &'static str,
    success: bool,
    message: String,
}

fn probe_outcome(id: &str, name: &str, probe: &SessionProbe, ok_message: &str) -> AccountOutcome {
    AccountOutcome {
        account_id: id.to_string(),
        name: name.to_string(),
        state: probe_state(probe),
        success: matches!(probe, SessionProbe::Ok),
        message: match probe {
            SessionProbe::Ok => ok_message.to_string(),
            other => probe_message(other),
        },
    }
}

fn skipped_outcome(id: &str, name: &str, message: &str) -> AccountOutcome {
    AccountOutcome {
        account_id: id.to_string(),
        name: name.to_string(),
        state: "skipped",
        success: false,
        message: message.to_string(),
    }
}

/// 签到结果也能反映会话状态：成功说明会话有效，提示"登录已过期"就标成过期。
fn register_sign_probe(probes: &mut Vec<(String, SessionProbe)>, id: &str, r: &SignResult) {
    if r.success {
        probes.push((id.to_string(), SessionProbe::Ok));
    } else if is_session_expired(&r.message) {
        probes.push((id.to_string(), SessionProbe::Expired(r.message.clone())));
    }
}

/// 单个账号的签到结果（批量接口用）。`upstream_*` 是上游原始判定，前端据此区分
/// `device_used` / `deviceAlreadyInUse` / `absent` 这类以前被压成一句"签到失败"的原因。
fn sign_json(
    account_id: &str,
    account_name: Option<&str>,
    result: &SignResult,
    with_distance: bool,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "account_id": account_id,
        "success": result.success,
        "message": result.message,
        "upstream_status": result.status,
        "upstream_code": result.http_code,
        "error_code": result.error_code,
    });
    if let Some(name) = account_name {
        value["name"] = serde_json::json!(name);
    }
    if with_distance {
        value["distance"] = serde_json::json!(result.distance);
    }
    value
}

fn sign_error_json(
    account_id: &str,
    error: &str,
    message: &str,
    with_distance: bool,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "account_id": account_id,
        "success": false,
        "message": message,
        "error": error,
    });
    if with_distance {
        value["distance"] = serde_json::Value::Null;
    }
    value
}

/// 同一学号只保留一条账号：重新登录时更新原账号的会话，保留原本的 id、启用状态、
/// 指纹和 deviceId，并清掉历史遗留的重复记录。返回是否更新了已有账号。
fn upsert_account(s: &mut Store, mut account: Account) -> bool {
    let mut updated = false;
    if account.student_id.is_empty() {
        s.accounts.push(account);
    } else {
        let positions: Vec<usize> = s
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.student_id == account.student_id)
            .map(|(i, _)| i)
            .collect();
        match positions.first().copied() {
            None => s.accounts.push(account),
            Some(first) => {
                let old_id = s.accounts[first].id.clone();
                let old_enabled = s.accounts[first].enabled;
                let old_device = s.accounts[first].device_id.clone();
                let old_bfp = s.accounts[first].bfp.clone();
                account.id = old_id;
                account.enabled = old_enabled;
                // 保留该账号自己的 device_id：换掉会让学校把它当成一台新设备
                account.device_id = old_device;
                if account.bfp.is_empty() {
                    account.bfp = old_bfp;
                }
                s.accounts[first] = account;
                for index in positions.into_iter().skip(1).rev() {
                    s.accounts.remove(index);
                }
                updated = true;
            }
        }
    }
    ensure_unique_device_ids(s);
    updated
}

/// 这次要签哪些账号：没指定就是全部启用的；指定了就按给定顺序取（含已停用的，
/// 手动补签不该被 enabled 拦住），找不到的账号作为失败结果返回。
fn select_accounts(
    accounts: &[Account],
    account_ids: Option<&[String]>,
) -> (Vec<Account>, Vec<(String, String)>) {
    let all_enabled = || -> Vec<Account> {
        accounts
            .iter()
            .filter(|a| a.enabled)
            .cloned()
            .collect::<Vec<_>>()
    };
    match account_ids {
        Some(ids) if !ids.is_empty() => {
            let mut picked = Vec::new();
            let mut missing = Vec::new();
            for id in ids {
                match accounts.iter().find(|a| a.id == *id) {
                    Some(account) => picked.push(account.clone()),
                    None => missing.push((id.clone(), "账号不存在".to_string())),
                }
            }
            let mut seen = HashSet::new();
            picked.retain(|a| seen.insert(a.id.clone()));
            (picked, missing)
        }
        _ => (all_enabled(), Vec::new()),
    }
}

fn validate_sign_target(account_id: &str, rollcall_id: &str) -> Result<(), String> {
    if account_id.trim().is_empty() {
        return Err("account_id 不能为空".into());
    }
    if rollcall_id.trim().is_empty() {
        return Err("rollcall_id 不能为空".into());
    }
    Ok(())
}

fn validate_coords(latitude: f64, longitude: f64) -> Result<(), String> {
    if !latitude.is_finite() || !(-90.0..=90.0).contains(&latitude) {
        return Err("latitude 无效：必须在 -90 到 90 之间".into());
    }
    if !longitude.is_finite() || !(-180.0..=180.0).contains(&longitude) {
        return Err("longitude 无效：必须在 -180 到 180 之间".into());
    }
    Ok(())
}

/// 数字签到码的取法：显式给了就用它；没给且开了自动提取就去点名里读；都没有就报错。
#[derive(Clone)]
enum NumberCode {
    Explicit(String),
    Auto,
    Missing,
}

fn resolve_number_code(code: &str, auto_number: bool) -> NumberCode {
    let trimmed = code.trim();
    if !trimmed.is_empty() {
        NumberCode::Explicit(trimmed.to_string())
    } else if auto_number {
        NumberCode::Auto
    } else {
        NumberCode::Missing
    }
}

fn find_account(
    st: &AppState,
    account_id: &str,
    with_distance: bool,
) -> Result<(Account, Settings), Response> {
    read_store(st, |s| match s.accounts.iter().find(|a| a.id == account_id) {
        Some(account) => Ok((account.clone(), s.settings.clone())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(sign_error_json(
                account_id,
                "account_not_found",
                "账号不存在",
                with_distance,
            )),
        )
            .into_response()),
    })
}

/// 保证这个账号有自己独有的 device_id：没有就现场生成并落盘。
fn ensure_account_device(st: &AppState, account: &mut Account) {
    if !account.device_id.trim().is_empty() {
        return;
    }
    let id = account.id.clone();
    let fresh = commit(st, |s| {
        let fresh = unique_device_id(&s.accounts);
        let mut changed = false;
        if let Some(x) = s.accounts.iter_mut().find(|x| x.id == id) {
            x.device_id = fresh.clone();
            changed = true;
        }
        (changed, fresh)
    });
    account.device_id = fresh;
}

// ---------------------------------------------------------------------------
// 各账号的签到动作（纯阻塞逻辑，便于单测与复用）
// ---------------------------------------------------------------------------

fn sign_qr_one(account: Account, raw: String) -> SignResult {
    core::sign_qr(account.cookie, account.device_id, raw)
}

fn sign_number_one(account: Account, rollcall_id: String, code: NumberCode) -> SignResult {
    let code = match code {
        NumberCode::Explicit(code) => code,
        NumberCode::Auto => {
            let found = core::get_number_code(account.cookie.clone(), rollcall_id.clone());
            if !found.ok {
                return SignResult::fail(found.message);
            }
            found.code
        }
        NumberCode::Missing => return SignResult::fail(MISSING_NUMBER_CODE),
    };
    core::sign_number(account.cookie, account.device_id, rollcall_id, code)
}

fn sign_radar_one(
    account: Account,
    rollcall_id: String,
    latitude: f64,
    longitude: f64,
) -> SignResult {
    let sign = core::sign_radar(
        account.cookie.clone(),
        account.device_id,
        rollcall_id.clone(),
        latitude,
        longitude,
    );
    if !sign.success {
        return sign; // 失败时保留 distance，三边定位要用
    }
    let verify = core::verify_signed(account.cookie, rollcall_id);
    let mut result = if verify.ok {
        SignResult::ok(verify.message)
    } else {
        SignResult::fail(verify.message)
    };
    result.status = verify.status;
    result.http_code = sign.http_code;
    result
}

fn sign_radar_empty_one(account: Account, rollcall_id: String) -> SignResult {
    let sign = core::sign_radar_empty(account.cookie.clone(), rollcall_id.clone());
    if !sign.success {
        return sign;
    }
    let verify = core::verify_signed(account.cookie, rollcall_id);
    let mut result = if verify.ok {
        SignResult::ok(verify.message)
    } else {
        SignResult::fail(verify.message)
    };
    result.status = verify.status;
    result.http_code = sign.http_code;
    result
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}

async fn admin_login(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminLogin>) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, json_result(false, "Origin 不匹配")).into_response();
    }
    if req.password != env_required("ADMIN_PASSWORD") {
        return (StatusCode::UNAUTHORIZED, json_result(false, "管理口令错误")).into_response();
    }
    // 只追加新 token，旧的继续有效：别的标签页/设备不会因为这里登录一次就掉线
    let token = commit(&st, |s| {
        let token = s.issue_token();
        (true, token)
    });
    let mut response = Json(serde_json::json!({"ok":true,"message":"登录成功","token":token}))
        .into_response();
    let cookie = format!("session={token}; HttpOnly; Secure; SameSite=Strict; Path=/");
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        Err(e) => eprintln!("生成会话 Cookie 失败：{e}"),
    }
    response
}

async fn accounts(State(st): State<AppState>, headers: HeaderMap) -> Response {
    read_store(&st, |s| {
        if !auth(&headers, s) {
            return unauthorized();
        }
        Json(accounts_json(s)).into_response()
    })
}

/// 主动探测每个账号的会话是否还有效（请求学校 /api/profile），结果缓存进 state.json，
/// 网页端据此显示「正常 / 已过期」。每个账号的结果都会返回，不只是数量。
async fn account_check(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let list = match authorized_accounts(&st, &headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let targets: Vec<(String, String)> = list
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    let jobs: Vec<Box<dyn FnOnce() -> SessionProbe + Send>> = list
        .into_iter()
        .map(|account| {
            let job: Box<dyn FnOnce() -> SessionProbe + Send> =
                Box::new(move || probe_from_account(core::get_account(account.cookie)));
            job
        })
        .collect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CHECK_DEADLINE_SECONDS);
    let outcomes = run_batched(
        CHECK_CONCURRENCY,
        Duration::from_secs(CHECK_JOB_TIMEOUT_SECONDS),
        Some(deadline),
        Vec::new(),
        jobs,
    )
    .await;
    let mut probes = Vec::new();
    let mut results = Vec::new();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let (id, name) = &targets[index];
        let probe = outcome.unwrap_or_else(|| SessionProbe::Failed(CHECK_TIMEOUT_HINT.to_string()));
        results.push(probe_outcome(id, name, &probe, "账号状态正常"));
        probes.push((id.clone(), probe));
    }
    apply_probes(&st, &probes);
    let accounts = read_store(&st, accounts_json);
    Json(serde_json::json!({"ok":true,"results":results,"accounts":accounts})).into_response()
}

async fn account_toggle(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    commit(&st, |s| {
        if !auth(&headers, s) {
            return (false, unauthorized());
        }
        match s.accounts.iter_mut().find(|a| a.id == id) {
            Some(account) => {
                account.enabled = !account.enabled;
                let enabled = account.enabled;
                (
                    true,
                    Json(serde_json::json!({"ok":true,"enabled":enabled})).into_response(),
                )
            }
            None => (
                false,
                (StatusCode::NOT_FOUND, json_result(false, "账号不存在")).into_response(),
            ),
        }
    })
}

async fn account_delete(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    commit(&st, |s| {
        if !auth(&headers, s) {
            return (false, unauthorized());
        }
        let before = s.accounts.len();
        s.accounts.retain(|a| a.id != id);
        if s.accounts.len() == before {
            return (
                false,
                (StatusCode::NOT_FOUND, json_result(false, "账号不存在")).into_response(),
            );
        }
        (true, json_result(true, "账号已删除").into_response())
    })
}

async fn school_login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SchoolLogin>,
) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    let username = req.username;
    let bfp = req.bfp.unwrap_or_default();
    let password = req.password;
    let remember = req.remember_me.unwrap_or(true);
    let cas_cookie = req.cas_cookie.unwrap_or_default();
    let login: LoginResult = match run_blocking({
        let u = username.clone();
        let b = bfp.clone();
        move || core::login(u, password, b, remember, cas_cookie)
    })
    .await
    {
        Some(r) => r,
        None => return (StatusCode::BAD_GATEWAY, json_result(false, "登录任务失败，请重试")).into_response(),
    };
    if !login.ok {
        return (StatusCode::BAD_REQUEST, json_result(false, login.message)).into_response();
    }
    let info: AccountInfo = match run_blocking({
        let cookie = login.cookie.clone();
        move || core::get_account(cookie)
    })
    .await
    {
        Some(info) => info,
        None => {
            return (
                StatusCode::BAD_GATEWAY,
                json_result(false, "获取账号信息失败，请重试"),
            )
                .into_response()
        }
    };
    let mut account = Account {
        id: String::new(),
        username,
        name: info.name.clone(),
        student_id: info.id.clone(),
        cookie: login.cookie,
        cas_cookie: login.cas_cookie,
        bfp,
        device_id: String::new(),
        enabled: true,
        expired: false,
        checked_at: unix_now(),
        status_detail: String::new(),
    };
    match probe_from_account(info) {
        SessionProbe::Ok => {}
        SessionProbe::Expired(message) => {
            account.expired = true;
            account.status_detail = message;
        }
        SessionProbe::Failed(message) => {
            account.status_detail = format!("状态检测失败：{message}");
        }
    }
    let updated = commit(&st, move |s| {
        account.id = unique_account_id(&s.accounts);
        account.device_id = unique_device_id(&s.accounts);
        let updated = upsert_account(s, account);
        (true, updated)
    });
    json_result(true, if updated { "登录成功，已更新原账号会话" } else { "登录成功" })
        .into_response()
}

async fn settings_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    read_store(&st, |s| {
        if !auth(&headers, s) {
            return unauthorized();
        }
        Json(s.settings.clone()).into_response()
    })
}

async fn settings_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SettingsInput>,
) -> Response {
    commit(&st, |s| {
        if !auth(&headers, s) {
            return (false, unauthorized());
        }
        if req.min_delay_seconds > 60
            || req.max_delay_seconds > 60
            || req.min_delay_seconds > req.max_delay_seconds
            || req.probes.len() > 20
            || req.probes.iter().any(|p| {
                !(-90.0..=90.0).contains(&p.latitude) || !(-180.0..=180.0).contains(&p.longitude)
            })
            || !matches!(req.radar_scheme.as_str(), "empty" | "trilateration")
        {
            return (
                false,
                (StatusCode::BAD_REQUEST, json_result(false, "设置范围无效")).into_response(),
            );
        }
        s.settings = Settings {
            min_delay_seconds: req.min_delay_seconds,
            max_delay_seconds: req.max_delay_seconds,
            auto_number: req.auto_number,
            radar_scheme: req.radar_scheme,
            probes: req.probes,
        };
        (true, json_result(true, "设置已保存").into_response())
    })
}

async fn session_status(State(st): State<AppState>, headers: HeaderMap) -> Response {
    read_store(&st, |s| {
        if !auth(&headers, s) {
            return unauthorized();
        }
        json_result(true, "已登录").into_response()
    })
}

async fn cas_start(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    match core::browser_start().await {
        Ok(flow) => {
            let base = env::var("CAS_PROXY_PUBLIC_URL")
                .unwrap_or_else(|_| "https://cqupt.xiaohundun.cc".into());
            Json(serde_json::json!({"ok":true,"flow_id":flow.id,"url":format!("{}{}",base.trim_end_matches('/'),flow.url)}))
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, json_result(false, e)).into_response(),
    }
}

async fn cas_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    let Some(done) = core::browser_take_completed(&id) else {
        return (StatusCode::ACCEPTED, json_result(false, "等待完成学校认证")).into_response();
    };
    let cookie = done.cookie.clone();
    let info = match run_blocking(move || core::get_account(cookie)).await {
        Some(info) => info,
        None => return (StatusCode::BAD_GATEWAY, json_result(false, "获取账号信息失败")).into_response(),
    };
    if !info.ok {
        return (StatusCode::BAD_GATEWAY, json_result(false, info.message)).into_response();
    }
    let mut account = Account {
        id: String::new(),
        username: info.id.clone(),
        name: info.name,
        student_id: info.id,
        cookie: done.cookie,
        cas_cookie: done.cas_cookie,
        bfp: String::new(),
        device_id: String::new(),
        enabled: true,
        expired: false,
        checked_at: unix_now(),
        status_detail: String::new(),
    };
    let updated = commit(&st, move |s| {
        account.id = unique_account_id(&s.accounts);
        account.device_id = unique_device_id(&s.accounts);
        let updated = upsert_account(s, account);
        (true, updated)
    });
    Json(serde_json::json!({"ok":true,"message":if updated {"学校网页登录成功，已更新原账号会话"} else {"学校网页登录成功，账号已保存"}}))
        .into_response()
}

async fn cas_root_proxy(
    Path(target): Path<String>,
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let flow = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("casflow="))
        .next()
        .unwrap_or("")
        .to_string();
    if flow.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            json_result(false, "认证代理会话缺失，请重新打开认证页面"),
        )
            .into_response();
    }
    cas_proxy_forward(flow, "ids.cqupt.edu.cn", format!("/authserver/{target}"), request).await
}

async fn cas_root_proxy_index(
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let flow = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("casflow="))
        .next()
        .unwrap_or("")
        .to_string();
    if flow.is_empty() {
        return (StatusCode::NOT_FOUND, json_result(false, "认证代理会话缺失")).into_response();
    }
    cas_proxy_forward(flow, "ids.cqupt.edu.cn", "/authserver".into(), request).await
}

async fn cas_proxy(
    Path((id, target)): Path<(String, String)>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let Some((host, rest)) = target.split_once('/') else {
        return (StatusCode::BAD_REQUEST, json_result(false, "代理路径错误")).into_response();
    };
    cas_proxy_forward(id, host, format!("/{rest}"), request).await
}

async fn cas_proxy_forward(
    id: String,
    host: &str,
    path: String,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let query = request.uri().query().map(str::to_string);
    let method = request.method().as_str().to_string();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let accept = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let requested_with = request
        .headers()
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match axum::body::to_bytes(request.into_body(), 4 * 1024 * 1024).await {
        Ok(b) => b.to_vec(),
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, json_result(false, "认证请求过大")).into_response(),
    };
    let upstream = match core::browser_forward(
        &id,
        host,
        &path,
        query.as_deref(),
        &method,
        body,
        content_type.as_deref(),
        accept.as_deref(),
        requested_with.as_deref(),
    )
    .await
    {
        Ok(x) => x,
        Err(e) => return (StatusCode::BAD_GATEWAY, json_result(false, e)).into_response(),
    };
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY));
    if let Some(ct) = upstream.content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    if let Some(loc) = upstream.location {
        builder = builder.header(header::LOCATION, loc);
    }
    let main_origin =
        env::var("MAIN_ORIGIN").unwrap_or_else(|_| "https://sign.xiaohundun.cc".into());
    builder = builder
        .header("Content-Security-Policy", format!("frame-ancestors {main_origin}"))
        .header("X-Content-Type-Options", "nosniff");
    if path.contains("/authserver/login") || path.contains("/authserver/") {
        builder = builder.header(
            header::SET_COOKIE,
            format!("casflow={id}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=600"),
        );
    }
    builder
        .body(axum::body::Body::from(upstream.body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok":true,"service":"winsignin"}))
}

enum RefreshOutcome {
    Updated {
        cookie: String,
        cas_cookie: String,
        probe: SessionProbe,
    },
    Failed {
        message: String,
        probe: SessionProbe,
    },
    Skipped {
        message: String,
    },
}

/// 单个账号的静默刷新（阻塞）。刷新失败多半是 CAS 的 TGT 到期，顺手探测一下旧的
/// lms 会话是否还活着，别把一个还能用的账号标成过期。
fn refresh_one(cas_cookie: String, cookie: String) -> RefreshOutcome {
    let r = core::refresh_session(cas_cookie);
    if !r.ok {
        let probe = probe_from_account(core::get_account(cookie));
        return RefreshOutcome::Failed {
            message: r.message,
            probe,
        };
    }
    let probe = probe_from_account(core::get_account(r.cookie.clone()));
    RefreshOutcome::Updated {
        cookie: r.cookie,
        cas_cookie: r.cas_cookie,
        probe,
    }
}

/// 用保存的统一认证票据静默刷新会话，逐个账号汇报结果：
/// 成功的更新 cookie，失败的说明原因（通常是 14 天的 TGT 到期，需要重新输密码）。
///
/// 落盘只做一次：以前是「每账号 save_store」并且全程持锁，等于给每个账号都加一次
/// 全表 fsync，并发请求全被拖死。
async fn refresh(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let list = match authorized_accounts(&st, &headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut targets: Vec<(String, String, String)> = Vec::new();
    let mut jobs: Vec<Box<dyn FnOnce() -> RefreshOutcome + Send>> = Vec::new();
    for account in list {
        targets.push((account.id.clone(), account.name.clone(), account.username.clone()));
        if account.cas_cookie.trim().is_empty() {
            jobs.push(Box::new(|| RefreshOutcome::Skipped {
                message: "没有保存统一认证票据，请用帐号密码重新登录".to_string(),
            }));
            continue;
        }
        let cas_cookie = account.cas_cookie.clone();
        let cookie = account.cookie.clone();
        jobs.push(Box::new(move || refresh_one(cas_cookie, cookie)));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(REFRESH_DEADLINE_SECONDS);
    let outcomes = run_batched(
        REFRESH_CONCURRENCY,
        Duration::from_secs(REFRESH_JOB_TIMEOUT_SECONDS),
        Some(deadline),
        Vec::new(),
        jobs,
    )
    .await;
    let mut updated = 0usize;
    let mut failed = Vec::new();
    let mut skipped = Vec::new();
    let mut results = Vec::new();
    let mut apply: Vec<(String, Option<(String, String)>, SessionProbe)> = Vec::new();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let (id, name, username) = targets[index].clone();
        match outcome {
            None => {
                let message = "刷新超时，请稍后重试".to_string();
                failed.push(serde_json::json!({
                    "id":id,"name":name,"username":username,"message":message
                }));
                results.push(AccountOutcome {
                    account_id: id,
                    name,
                    state: "failed",
                    success: false,
                    message,
                });
            }
            Some(RefreshOutcome::Skipped { message }) => {
                skipped.push(serde_json::json!({
                    "id":id,"name":name,"username":username,"message":message
                }));
                results.push(skipped_outcome(&id, &name, &message));
            }
            Some(RefreshOutcome::Failed { message, probe }) => {
                failed.push(serde_json::json!({
                    "id":id,"name":name,"username":username,"message":message
                }));
                // 刷新失败但旧会话还活着：状态记 ok，同时把"自动刷新已失效"讲清楚
                results.push(match &probe {
                    SessionProbe::Ok => AccountOutcome {
                        account_id: id.clone(),
                        name: name.clone(),
                        state: "ok",
                        success: true,
                        message: "当前会话仍有效，但统一认证票据已过期，自动刷新失效，建议重新登录".to_string(),
                    },
                    other => probe_outcome(&id, &name, other, ""),
                });
                apply.push((id, None, probe));
            }
            Some(RefreshOutcome::Updated {
                cookie,
                cas_cookie,
                probe,
            }) => {
                updated += 1;
                results.push(probe_outcome(&id, &name, &probe, "会话已刷新"));
                apply.push((id, Some((cookie, cas_cookie)), probe));
            }
        }
    }
    // 所有账号只落一次盘
    if !apply.is_empty() {
        commit(&st, |s| {
            let mut changed = false;
            for (id, replace, probe) in &apply {
                let Some(x) = s.accounts.iter_mut().find(|x| x.id == *id) else {
                    continue;
                };
                if let Some((cookie, cas_cookie)) = replace {
                    if x.cookie != *cookie || x.cas_cookie != *cas_cookie {
                        changed = true;
                    }
                    x.cookie = cookie.clone();
                    x.cas_cookie = cas_cookie.clone();
                }
                let before = (x.expired, x.status_detail.clone());
                apply_probe(x, probe);
                if (x.expired, x.status_detail.clone()) != before {
                    changed = true;
                }
            }
            (changed, ())
        });
    }
    let accounts = read_store(&st, accounts_json);
    Json(serde_json::json!({
        "ok":true,"updated":updated,"failed":failed,"skipped":skipped,
        "results":results,"accounts":accounts
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
struct RollcallsQuery {
    monitor: Option<String>,
}

impl RollcallsQuery {
    fn monitor_enabled(&self) -> bool {
        matches!(self.monitor.as_deref(), Some("1" | "true"))
    }
}

#[derive(Serialize)]
struct AccountHealth {
    account_id: String,
    name: String,
    state: &'static str,
    message: &'static str,
}

fn account_health(account_id: &str, name: &str, probe: &SessionProbe) -> AccountHealth {
    // Never expose upstream errors or session details through the read-only monitor API.
    let (state, message) = match probe {
        SessionProbe::Ok => ("ok", "账号状态正常"),
        SessionProbe::Expired(_) => ("expired", "登录已过期，请重新登录"),
        SessionProbe::Failed(_) => ("failed", "暂时无法获取点名，请稍后重试"),
    };
    AccountHealth {
        account_id: account_id.into(),
        name: name.into(),
        state,
        message,
    }
}

async fn rollcalls(
    State(st): State<AppState>,
    Query(query): Query<RollcallsQuery>,
    headers: HeaderMap,
) -> Response {
    let list = match authorized_rollcall_accounts(&st, &headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut grouped: HashMap<String, serde_json::Value> = HashMap::new();
    // Start every account before awaiting any result, so request timeouts do not add up serially.
    let handles: Vec<_> = list
        .into_iter()
        .filter(|a| a.enabled)
        .map(|a| {
            let account_id = a.id;
            let account_name = a.name;
            let handle = tokio::task::spawn_blocking(move || core::get_rollcalls(a.cookie));
            (account_id, account_name, handle)
        })
        .collect();
    let accounts_checked = handles.len();
    let mut complete = accounts_checked > 0;
    let mut probes = Vec::new();
    let mut health = Vec::new();
    for (account_id, account_name, handle) in handles {
        let r: RollcallsResult = match handle.await {
            Ok(r) => r,
            Err(_) => RollcallsResult {
                ok: false,
                message: "点名查询任务失败".into(),
                rollcalls: Vec::new(),
            },
        };
        let probe = if r.ok {
            SessionProbe::Ok
        } else if is_session_expired(&r.message) {
            SessionProbe::Expired(r.message.clone())
        } else {
            SessionProbe::Failed(r.message.clone())
        };
        health.push(account_health(&account_id, &account_name, &probe));
        probes.push((account_id.clone(), probe));
        if !r.ok {
            complete = false;
            continue;
        }
        for x in r.rollcalls {
            let rid = x.id.clone();
            let entry = grouped.entry(rid.clone()).or_insert_with(|| {
                serde_json::json!({"id":rid,"course_title":x.course_title,"title":x.title,"is_number":x.is_number,"is_radar":x.is_radar,"is_expired":x.is_expired,"status":x.status,"rollcall_time":x.rollcall_time,"accounts":[]})
            });
            if x.is_expired {
                entry["is_expired"] = serde_json::Value::Bool(true);
            }
            if let Some(accounts) = entry.get_mut("accounts").and_then(|v| v.as_array_mut()) {
                accounts.push(serde_json::json!({"account_id":account_id,"name":account_name,"status":x.status}));
            }
        }
    }
    apply_probes(&st, &probes);
    let rollcalls = grouped.into_values().collect::<Vec<_>>();
    let mut response = if query.monitor_enabled() {
        Json(serde_json::json!({"rollcalls":rollcalls,"complete":complete,"account_health":health}))
            .into_response()
    } else {
        Json(rollcalls).into_response()
    };
    response.headers_mut().insert(
        "X-Rollcalls-Complete",
        HeaderValue::from_static(if complete { "true" } else { "false" }),
    );
    response
}

/// 批量扫码签到。内部**串行**（风控最敏感），按 settings 的错峰间隔逐个执行，
/// 每个账号独立成功/失败，单次请求不设硬超时。
/// 耗时可控靠前端分批：`account_ids` 每批 3 个左右多次调用。
async fn qr(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<QrReq>) -> Response {
    let raw = req.raw.trim().to_string();
    if raw.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            json_result(false, "签到二维码内容为空"),
        )
            .into_response();
    }
    // 二维码签到不读错峰设置：它拼的是"在二维码过期前把请求都发出去"
    let (accounts, missing, _settings) =
        match sign_accounts(&st, &headers, req.account_ids.as_deref()) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let ids: Vec<(String, String)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    let jobs: Vec<Box<dyn FnOnce() -> SignResult + Send>> = accounts
        .into_iter()
        .map(|account| {
            let raw = raw.clone();
            let job: Box<dyn FnOnce() -> SignResult + Send> =
                Box::new(move || sign_qr_one(account, raw));
            job
        })
        .collect();
    // 二维码几秒就换一张：这里放开并发、不排队等错峰，先把所有账号的请求都发出去。
    let results = run_batched(
        QR_BATCH_SIZE,
        Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
        None,
        Vec::new(),
        jobs,
    )
    .await;
    let mut out = Vec::new();
    let mut probes = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        let (id, name) = &ids[index];
        match result {
            Some(result) => {
                register_sign_probe(&mut probes, id, &result);
                out.push(sign_json(id, Some(name), &result, true));
            }
            None => out.push(sign_json(
                id,
                Some(name),
                &SignResult::fail(SIGN_TIMEOUT_HINT),
                true,
            )),
        }
    }
    for (id, message) in missing {
        out.push(sign_error_json(&id, "account_not_found", &message, false));
    }
    apply_probes(&st, &probes);
    Json(out).into_response()
}

/// 单账号扫码补签：批量里失败/超时的账号靠这个逐个补。
async fn qr_account(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QrAccountReq>,
) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    let account_id = req.account_id.trim().to_string();
    let raw = req.raw.trim().to_string();
    if account_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json("", "invalid_request", "account_id 不能为空", false)),
        )
            .into_response();
    }
    if raw.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json(
                &account_id,
                "invalid_request",
                "签到二维码内容为空",
                false,
            )),
        )
            .into_response();
    }
    let (mut account, _settings) = match find_account(&st, &account_id, false) {
        Ok(x) => x,
        Err(r) => return r,
    };
    ensure_account_device(&st, &mut account);
    single_sign(&st, account, false, move |account| sign_qr_one(account, raw)).await
}

async fn number(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<NumberReq>) -> Response {
    let (accounts, missing, settings) = match sign_accounts(&st, &headers, None) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let _ = missing;
    let rollcall_id = req.rollcall_id.trim().to_string();
    if rollcall_id.is_empty() {
        return (StatusCode::BAD_REQUEST, json_result(false, "rollcall_id 不能为空")).into_response();
    }
    let code = resolve_number_code(&req.code, settings.auto_number);
    let ids: Vec<(String, String)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    let jobs: Vec<Box<dyn FnOnce() -> SignResult + Send>> = accounts
        .into_iter()
        .map(|account| {
            let rollcall_id = rollcall_id.clone();
            let code = code.clone();
            let job: Box<dyn FnOnce() -> SignResult + Send> =
                Box::new(move || sign_number_one(account, rollcall_id, code));
            job
        })
        .collect();
    let delays = stagger_plan(&settings, jobs.len());
    let results = run_batched(
        SIGN_BATCH_SIZE,
        Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
        None,
        delays,
        jobs,
    )
    .await;
    let mut out = Vec::new();
    let mut probes = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        let (id, name) = &ids[index];
        match result {
            Some(result) => {
                register_sign_probe(&mut probes, id, &result);
                out.push(sign_json(id, Some(name), &result, true));
            }
            None => out.push(sign_json(
                id,
                Some(name),
                &SignResult::fail(SIGN_TIMEOUT_HINT),
                true,
            )),
        }
    }
    apply_probes(&st, &probes);
    Json(out).into_response()
}

async fn radar_empty(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<NumberReq>) -> Response {
    let (accounts, _missing, settings) = match sign_accounts(&st, &headers, None) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let rollcall_id = req.rollcall_id.trim().to_string();
    if rollcall_id.is_empty() {
        return (StatusCode::BAD_REQUEST, json_result(false, "rollcall_id 不能为空")).into_response();
    }
    let ids: Vec<(String, String)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    let jobs: Vec<Box<dyn FnOnce() -> SignResult + Send>> = accounts
        .into_iter()
        .map(|account| {
            let rollcall_id = rollcall_id.clone();
            let job: Box<dyn FnOnce() -> SignResult + Send> =
                Box::new(move || sign_radar_empty_one(account, rollcall_id));
            job
        })
        .collect();
    let delays = stagger_plan(&settings, jobs.len());
    let results = run_batched(
        SIGN_BATCH_SIZE,
        Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
        None,
        delays,
        jobs,
    )
    .await;
    let mut out = Vec::new();
    let mut probes = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        let (id, name) = &ids[index];
        match result {
            Some(result) => {
                register_sign_probe(&mut probes, id, &result);
                out.push(sign_json(id, Some(name), &result, false));
            }
            None => out.push(sign_json(
                id,
                Some(name),
                &SignResult::fail(SIGN_TIMEOUT_HINT),
                false,
            )),
        }
    }
    apply_probes(&st, &probes);
    Json(out).into_response()
}

async fn radar(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RadarReq>) -> Response {
    let (accounts, _missing, settings) = match sign_accounts(&st, &headers, None) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let rollcall_id = req.rollcall_id.trim().to_string();
    if rollcall_id.is_empty() {
        return (StatusCode::BAD_REQUEST, json_result(false, "rollcall_id 不能为空")).into_response();
    }
    if let Err(message) = validate_coords(req.latitude, req.longitude) {
        return (StatusCode::BAD_REQUEST, json_result(false, message)).into_response();
    }
    let ids: Vec<(String, String)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    let jobs: Vec<Box<dyn FnOnce() -> SignResult + Send>> = accounts
        .into_iter()
        .map(|account| {
            let rollcall_id = rollcall_id.clone();
            let latitude = req.latitude;
            let longitude = req.longitude;
            let job: Box<dyn FnOnce() -> SignResult + Send> =
                Box::new(move || sign_radar_one(account, rollcall_id, latitude, longitude));
            job
        })
        .collect();
    let delays = stagger_plan(&settings, jobs.len());
    let results = run_batched(
        SIGN_BATCH_SIZE,
        Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
        None,
        delays,
        jobs,
    )
    .await;
    let mut out = Vec::new();
    let mut probes = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        let (id, name) = &ids[index];
        match result {
            Some(result) => {
                register_sign_probe(&mut probes, id, &result);
                out.push(sign_json(id, Some(name), &result, true));
            }
            None => out.push(sign_json(
                id,
                Some(name),
                &SignResult::fail(SIGN_TIMEOUT_HINT),
                true,
            )),
        }
    }
    apply_probes(&st, &probes);
    Json(out).into_response()
}

/// 单账号签到的公共收尾：限时执行、回写会话状态、按契约输出字段。
async fn single_sign(
    st: &AppState,
    mut account: Account,
    with_distance: bool,
    job: impl FnOnce(Account) -> SignResult + Send + 'static,
) -> Response {
    ensure_account_device(st, &mut account);
    let id = account.id.clone();
    let name = account.name.clone();
    let outcome = tokio::time::timeout(
        Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
        tokio::task::spawn_blocking(move || job(account)),
    )
    .await;
    let result = match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            eprintln!("单账号签到任务失败：{e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(sign_error_json(&id, "internal_error", "签到任务失败，请重试", with_distance)),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(sign_error_json(&id, "timeout", SIGN_TIMEOUT_HINT, with_distance)),
            )
                .into_response();
        }
    };
    let mut probes = Vec::new();
    register_sign_probe(&mut probes, &id, &result);
    apply_probes(st, &probes);
    // 只带契约里约定的字段（name 供前端显示用），不再额外塞内部信息
    let mut value = sign_json(&id, Some(&name), &result, with_distance);
    if let Some(object) = value.as_object_mut() {
        object.remove("name");
    }
    Json(value).into_response()
}

/// POST /api/sign/number/account —— 单账号数字签到。
async fn number_account(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NumberAccountReq>,
) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    let account_id = req.account_id.trim().to_string();
    let rollcall_id = req.rollcall_id.trim().to_string();
    if let Err(message) = validate_sign_target(&account_id, &rollcall_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json(
                &account_id,
                "invalid_request",
                &message,
                false,
            )),
        )
            .into_response();
    }
    let (account, settings) = match find_account(&st, &account_id, false) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let code = resolve_number_code(&req.code, settings.auto_number);
    if matches!(code, NumberCode::Missing) {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json(
                &account_id,
                "invalid_request",
                MISSING_NUMBER_CODE,
                false,
            )),
        )
            .into_response();
    }
    single_sign(&st, account, false, move |account| {
        sign_number_one(account, rollcall_id, code)
    })
    .await
}

/// POST /api/sign/radar/account —— 单账号位置签到。
async fn radar_account(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RadarAccountReq>,
) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    let account_id = req.account_id.trim().to_string();
    let rollcall_id = req.rollcall_id.trim().to_string();
    if let Err(message) = validate_sign_target(&account_id, &rollcall_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json(
                &account_id,
                "invalid_request",
                &message,
                true,
            )),
        )
            .into_response();
    }
    if let Err(message) = validate_coords(req.latitude, req.longitude) {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json(
                &account_id,
                "invalid_request",
                &message,
                true,
            )),
        )
            .into_response();
    }
    let (account, _settings) = match find_account(&st, &account_id, true) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let latitude = req.latitude;
    let longitude = req.longitude;
    single_sign(&st, account, true, move |account| {
        sign_radar_one(account, rollcall_id, latitude, longitude)
    })
    .await
}

/// POST /api/sign/radar/empty/account —— 单账号空答案雷达签到。
async fn radar_empty_account(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EmptyAccountReq>,
) -> Response {
    if !authorized(&st, &headers) {
        return unauthorized();
    }
    let account_id = req.account_id.trim().to_string();
    let rollcall_id = req.rollcall_id.trim().to_string();
    if let Err(message) = validate_sign_target(&account_id, &rollcall_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(sign_error_json(
                &account_id,
                "invalid_request",
                &message,
                false,
            )),
        )
            .into_response();
    }
    let (account, _settings) = match find_account(&st, &account_id, false) {
        Ok(x) => x,
        Err(r) => return r,
    };
    single_sign(&st, account, false, move |account| {
        sign_radar_empty_one(account, rollcall_id)
    })
    .await
}

/// 探测点的几何中心：位置类点名没给坐标时的默认落点。
fn probe_center(probes: &[RadarProbe]) -> Option<(f64, f64)> {
    if probes.is_empty() {
        return None;
    }
    let count = probes.len() as f64;
    let lat = probes.iter().map(|p| p.latitude).sum::<f64>() / count;
    let lon = probes.iter().map(|p| p.longitude).sum::<f64>() / count;
    validate_coords(lat, lon).ok().map(|_| (lat, lon))
}

/// 一个点名在"一键签到"里的状态汇总（用于把跳过原因讲清楚）。
struct PendingRollcall {
    rollcall_id: String,
    course_title: String,
    title: String,
    kind: RollcallKind,
    is_expired: bool,
    /// (account_id, name, 该账号在这场点名里的状态)
    accounts: Vec<(String, String, String)>,
}

fn rollcall_kind(is_number: bool, is_radar: bool) -> RollcallKind {
    if is_number {
        RollcallKind::Number
    } else if is_radar {
        RollcallKind::Radar
    } else {
        RollcallKind::Qr
    }
}

/// 从"每个账号看到的点名"里挑出真正要签的任务。
///
/// 只挑**未结束**的点名里、**状态不是已签到**的账号；这样跨课程批量签到不会
/// 对已经签过的人重复提交（重复提交会触发上游 device_used / 风控）。
fn plan_all_signs(
    grouped: &HashMap<String, PendingRollcall>,
    filter: Option<&[String]>,
    cap: usize,
) -> (Vec<AllSignJob>, Vec<serde_json::Value>, bool) {
    let mut jobs = Vec::new();
    let mut skipped = Vec::new();
    let mut truncated = false;
    // 固定顺序，避免每次请求结果顺序抖动
    let mut ids: Vec<&String> = grouped.keys().collect();
    ids.sort();
    for rid in ids {
        let Some(entry) = grouped.get(rid) else { continue };
        // 空数组与不传一致（都表示"全部"），与 /api/sign/qr 的 account_ids 语义对齐，
        // 避免两个批量接口对空数组的处理相反。
        let wanted = match filter {
            None => true,
            Some(list) => list.is_empty() || list.iter().any(|x| x == rid),
        };
        if !wanted {
            continue;
        }
        if entry.is_expired {
            skipped.push(serde_json::json!({
                "rollcall_id":entry.rollcall_id,"course_title":entry.course_title,
                "title":entry.title,"kind":entry.kind.as_str(),"reason":"已结束"
            }));
            continue;
        }
        let mut pending: Vec<&(String, String, String)> = entry
            .accounts
            .iter()
            .filter(|(_, _, status)| !status_is_signed(status))
            .collect();
        if pending.is_empty() {
            skipped.push(serde_json::json!({
                "rollcall_id":entry.rollcall_id,"course_title":entry.course_title,
                "title":entry.title,"kind":entry.kind.as_str(),"reason":"所有账号都已签到"
            }));
            continue;
        }
        // 同一个账号理论上只出现一次，这里仍然去重，防止上游返回重复条目
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        pending.dedup_by(|a, b| a.0 == b.0);
        for (account_id, account_name, _) in pending {
            if jobs.len() >= cap {
                truncated = true;
                break;
            }
            jobs.push(AllSignJob {
                rollcall_id: entry.rollcall_id.clone(),
                kind: entry.kind,
                account_id: account_id.clone(),
                account_name: account_name.clone(),
            });
        }
        if truncated {
            break;
        }
    }
    (jobs, skipped, truncated)
}

/// 上游把"已签到"表达成 on_call_fine。
///
/// 不把 `on_call` 也算进来：`verify_signed` 只认 `on_call_fine`，AstrBot 插件同样只把
/// `on_call_fine` 映射成"已签到"。把 `on_call` 当已签的风险是**漏签**（用户以为签了其实没签），
/// 而多签一次的代价只是上游回一句"已签到"，所以这里取保守判定。
fn status_is_signed(status: &str) -> bool {
    status.trim() == "on_call_fine"
}

/// POST /api/sign/all —— 跨课程一键签到。
///
/// 把当前**所有进行中的点名**按各自类型（数字/位置/二维码）对**能看到它的账号**签到，
/// 已签到或已结束的自动跳过。二维码类点名需要调用方带上 `qr_raw`，否则只报告"需要扫码"。
async fn sign_all(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SignAllReq>,
) -> Response {
    let (accounts, _missing, settings) = match sign_accounts(&st, &headers, None) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if accounts.is_empty() {
        return (StatusCode::BAD_REQUEST, json_result(false, "没有启用的账号")).into_response();
    }

    // 坐标：显式传入优先，否则退回设置里探测点的中心
    let explicit_coords = match (req.latitude, req.longitude) {
        (Some(latitude), Some(longitude)) => match validate_coords(latitude, longitude) {
            Ok(()) => Some((latitude, longitude)),
            Err(message) => {
                return (StatusCode::BAD_REQUEST, json_result(false, message)).into_response()
            }
        },
        _ => None,
    };
    let coords = explicit_coords.or_else(|| probe_center(&settings.probes));
    let qr_raw = req.qr_raw.unwrap_or_default();
    let qr_raw = qr_raw.trim().to_string();

    // 1. 并发拉每个账号的点名（先全部启动，避免超时叠加）
    let handles: Vec<_> = accounts
        .iter()
        .map(|a| {
            let id = a.id.clone();
            let name = a.name.clone();
            let cookie = a.cookie.clone();
            (
                id,
                name,
                tokio::task::spawn_blocking(move || core::get_rollcalls(cookie)),
            )
        })
        .collect();

    let mut grouped: HashMap<String, PendingRollcall> = HashMap::new();
    let mut probes = Vec::new();
    let mut unreadable = Vec::new();
    for (id, name, handle) in handles {
        let r: RollcallsResult = match handle.await {
            Ok(r) => r,
            Err(_) => RollcallsResult {
                ok: false,
                message: "点名查询任务失败".into(),
                rollcalls: Vec::new(),
            },
        };
        let probe = if r.ok {
            SessionProbe::Ok
        } else if is_session_expired(&r.message) {
            SessionProbe::Expired(r.message.clone())
        } else {
            SessionProbe::Failed(r.message.clone())
        };
        probes.push((id.clone(), probe));
        if !r.ok {
            unreadable.push(serde_json::json!({
                "account_id":id,"name":name,"message":r.message
            }));
            continue;
        }
        for x in r.rollcalls {
            let entry = grouped
                .entry(x.id.clone())
                .or_insert_with(|| PendingRollcall {
                    rollcall_id: x.id.clone(),
                    course_title: x.course_title.clone(),
                    title: x.title.clone(),
                    kind: rollcall_kind(x.is_number, x.is_radar),
                    is_expired: x.is_expired,
                    accounts: Vec::new(),
                });
            if x.is_expired {
                entry.is_expired = true;
            }
            entry.accounts.push((id.clone(), name.clone(), x.status.clone()));
        }
    }
    apply_probes(&st, &probes);

    // 2. 规划任务
    let (jobs, mut skipped, truncated) =
        plan_all_signs(&grouped, req.rollcall_ids.as_deref(), MAX_ALL_SIGN_JOBS);

    // 二维码里编码的是"哪一场点名"，而 sign_qr 的提交地址取自二维码本身。
    // 拿 A 课的二维码去签 B 课会静默签错课，所以这里先解析出来做匹配校验。
    let qr_target = if qr_raw.is_empty() {
        None
    } else {
        core::qr_rollcall_id(&qr_raw)
    };
    let needs_radar_coords = settings.radar_scheme != "empty";

    // 筛选：任何被排除的点名都必须进 skipped 并写明原因，否则用户会以为已经签完。
    let mut runnable: Vec<AllSignJob> = Vec::new();
    let mut blocked: Vec<(String, &'static str, String)> = Vec::new();
    for job in jobs {
        match job.kind {
            RollcallKind::Number => {}
            RollcallKind::Radar => {
                // 空答案方案根本不读坐标，所以只有非 empty 方案才要求坐标
                if needs_radar_coords && coords.is_none() {
                    blocked.push((
                        job.rollcall_id.clone(),
                        "radar",
                        "位置签到缺少坐标：请在签到设置里填写探测点".to_string(),
                    ));
                    continue;
                }
            }
            RollcallKind::Qr => {
                if qr_raw.is_empty() {
                    blocked.push((
                        job.rollcall_id.clone(),
                        "qr",
                        "二维码类点名需要先扫码或粘贴二维码原文".to_string(),
                    ));
                    continue;
                }
                match qr_target.as_deref() {
                    Some(target) if target == job.rollcall_id => {}
                    Some(_) if req.allow_qr_mismatch => {
                        // 调用方明确要求"硬试"：上游若认这个组合就会成功，
                        // 不认就是一条明确的失败原因，不再由我们提前拦下。
                    }
                    Some(_) => {
                        blocked.push((
                            job.rollcall_id.clone(),
                            "qr",
                            "二维码与这场点名不匹配：它属于另一场点名（如需强行尝试请开启 allow_qr_mismatch）"
                                .to_string(),
                        ));
                        continue;
                    }
                    None => {
                        blocked.push((
                            job.rollcall_id.clone(),
                            "qr",
                            "无法从二维码原文解析出点名".to_string(),
                        ));
                        continue;
                    }
                }
            }
        }
        runnable.push(job);
    }
    let mut blocked_seen: Vec<String> = Vec::new();
    for (rid, kind, reason) in blocked {
        // 同一场点名可能因为多个账号各报一次，这里按 (点名, 原因) 去重
        let key = format!("{rid}\u{0}{kind}\u{0}{reason}");
        if blocked_seen.contains(&key) {
            continue;
        }
        blocked_seen.push(key);
        if let Some(entry) = grouped.get(&rid) {
            skipped.push(serde_json::json!({
                "rollcall_id":entry.rollcall_id,"course_title":entry.course_title,
                "title":entry.title,"kind":kind,"reason":reason
            }));
        }
    }

    // 3. 按账号取出可用的 Account（含 cookie / device_id）。
    //    注意用 get + clone：同一个账号要能同时承担多门课的任务，
    //    早先的 pool.remove 会让每个账号一次调用只签得动一门课。
    let mut pool: HashMap<String, Account> = accounts
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect();
    for account in pool.values_mut() {
        ensure_account_device(&st, account);
    }
    let mut prepared: Vec<(AllSignJob, Account)> = Vec::new();
    for job in runnable {
        match pool.get(&job.account_id) {
            Some(account) => prepared.push((job, account.clone())),
            None => skipped.push(serde_json::json!({
                "rollcall_id":job.rollcall_id,"kind":job.kind.as_str(),
                "reason":"账号不存在或已被删除"
            })),
        }
    }
    if truncated {
        // 这条是"整体"提示而不是某一场点名，用 code 标出来，前端据此只显示一条横幅
        skipped.push(serde_json::json!({
            "code":"truncated","rollcall_id":"","kind":"","reason":format!(
                "本次任务数超过单次上限 {}，剩余点名未处理，请再点一次继续", MAX_ALL_SIGN_JOBS)
        }));
    }

    // 4. 执行。二维码类拼的是"在二维码过期前把请求发出去"，走高速通道（并发、不排队等错峰）；
    //    数字/位置类不赶时间，仍然串行 + 错峰，避免触发学校侧风控。
    let make_job = |job: &AllSignJob, account: &Account| -> Box<dyn FnOnce() -> SignResult + Send> {
        let account = account.clone();
        let rollcall_id = job.rollcall_id.clone();
        let kind = job.kind;
        let auto_number = settings.auto_number;
        let radar_scheme = settings.radar_scheme.clone();
        let raw = qr_raw.clone();
        Box::new(move || match kind {
            RollcallKind::Number => {
                sign_number_one(account, rollcall_id, resolve_number_code("", auto_number))
            }
            RollcallKind::Radar => {
                if radar_scheme == "empty" {
                    sign_radar_empty_one(account, rollcall_id)
                } else if let Some((latitude, longitude)) = coords {
                    sign_radar_one(account, rollcall_id, latitude, longitude)
                } else {
                    SignResult::fail("位置签到缺少坐标")
                }
            }
            RollcallKind::Qr => sign_qr_one(account, raw),
        })
    };

    let mut outcomes: Vec<Option<SignResult>> = (0..prepared.len()).map(|_| None).collect();

    // 4a. 二维码任务：并发、无错峰
    let qr_indexes: Vec<usize> = prepared
        .iter()
        .enumerate()
        .filter(|(_, (job, _))| job.kind == RollcallKind::Qr)
        .map(|(index, _)| index)
        .collect();
    if !qr_indexes.is_empty() {
        let jobs: Vec<_> = qr_indexes
            .iter()
            .map(|&index| make_job(&prepared[index].0, &prepared[index].1))
            .collect();
        let mut got = run_batched(
            QR_BATCH_SIZE,
            Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
            None,
            Vec::new(),
            jobs,
        )
        .await;
        for (slot, &index) in qr_indexes.iter().enumerate() {
            outcomes[index] = got.get_mut(slot).and_then(|x| x.take());
        }
    }

    // 4b. 其余任务：串行 + 错峰
    let slow_indexes: Vec<usize> = prepared
        .iter()
        .enumerate()
        .filter(|(_, (job, _))| job.kind != RollcallKind::Qr)
        .map(|(index, _)| index)
        .collect();
    if !slow_indexes.is_empty() {
        let delays = stagger_plan(&settings, slow_indexes.len());
        let jobs: Vec<_> = slow_indexes
            .iter()
            .map(|&index| make_job(&prepared[index].0, &prepared[index].1))
            .collect();
        let mut got = run_batched(
            SIGN_BATCH_SIZE,
            Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
            Some(tokio::time::Instant::now() + Duration::from_secs(ALL_SIGN_DEADLINE_SECONDS)),
            delays,
            jobs,
        )
        .await;
        for (slot, &index) in slow_indexes.iter().enumerate() {
            outcomes[index] = got.get_mut(slot).and_then(|x| x.take());
        }
    }

    // 5. 按点名分组输出
    let mut by_rollcall: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let mut sign_probes = Vec::new();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let (job, _) = &prepared[index];
        let result = outcome.unwrap_or_else(|| SignResult::fail(SIGN_TIMEOUT_HINT));
        register_sign_probe(&mut sign_probes, &job.account_id, &result);
        let mut row = sign_json(&job.account_id, Some(&job.account_name), &result, job.kind == RollcallKind::Radar);
        if let Some(obj) = row.as_object_mut() {
            obj.insert("rollcall_id".into(), serde_json::json!(job.rollcall_id));
            obj.insert("kind".into(), serde_json::json!(job.kind.as_str()));
        }
        by_rollcall
            .entry(job.rollcall_id.clone())
            .or_default()
            .push(row);
    }
    apply_probes(&st, &sign_probes);

    let mut rollcall_out = Vec::new();
    let mut signed = 0usize;
    let mut failed = 0usize;
    let mut ids: Vec<&String> = by_rollcall.keys().collect();
    ids.sort();
    for rid in ids {
        let Some(rows) = by_rollcall.get(rid) else { continue };
        let Some(entry) = grouped.get(rid) else { continue };
        let ok_count = rows
            .iter()
            .filter(|r| r.get("success").and_then(|v| v.as_bool()).unwrap_or(false))
            .count();
        signed += ok_count;
        failed += rows.len() - ok_count;
        rollcall_out.push(serde_json::json!({
            "rollcall_id":entry.rollcall_id,
            "course_title":entry.course_title,
            "title":entry.title,
            "kind":entry.kind.as_str(),
            "results":rows,
        }));
    }

    Json(serde_json::json!({
        "ok": true,
        "message": format!("已处理 {} 场点名", rollcall_out.len()),
        "rollcalls": rollcall_out,
        "skipped": skipped,
        "unreadable_accounts": unreadable,
        "truncated": truncated,
        "summary": {
            "rollcalls": rollcall_out.len(),
            "signed": signed,
            "failed": failed,
            "skipped": skipped.len(),
        }
    }))
    .into_response()
}

async fn radar_triangulate(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RadarTriangulateReq>,
) -> Response {
    if req.probes.len() < 3
        || req.probes.iter().take(3).any(|p| {
            !(-90.0..=90.0).contains(&p.latitude) || !(-180.0..=180.0).contains(&p.longitude)
        })
    {
        return (
            StatusCode::BAD_REQUEST,
            json_result(false, "至少需要 3 个有效探测点"),
        )
            .into_response();
    }
    let (accounts, _missing, settings) = match sign_accounts(&st, &headers, None) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let probe_account = match accounts.first().cloned() {
        Some(account) => account,
        None => return (StatusCode::BAD_REQUEST, json_result(false, "没有启用的账号")).into_response(),
    };
    let rollcall_id = req.rollcall_id.trim().to_string();
    if rollcall_id.is_empty() {
        return (StatusCode::BAD_REQUEST, json_result(false, "rollcall_id 不能为空")).into_response();
    }
    let mut distances = Vec::new();
    for point in req.probes.iter().take(3) {
        let cookie = probe_account.cookie.clone();
        let device = probe_account.device_id.clone();
        let rid = rollcall_id.clone();
        let latitude = point.latitude;
        let longitude = point.longitude;
        let Some(sign) = run_blocking(move || core::sign_radar(cookie, device, rid, latitude, longitude)).await
        else {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"ok":false,"message":"雷达探测任务失败","target":null,"results":[]})),
            )
                .into_response();
        };
        if sign.success {
            let cookie = probe_account.cookie.clone();
            let rid = rollcall_id.clone();
            let (ok, message) = match run_blocking(move || core::verify_signed(cookie, rid)).await {
                Some(verify) => (verify.ok, verify.message),
                None => (false, "签到确认失败".to_string()),
            };
            return Json(serde_json::json!({"ok":ok,"message":message,"target":null,"results":[]}))
                .into_response();
        }
        match sign.distance {
            Some(distance) => distances.push(distance),
            None => {
                return Json(serde_json::json!({"ok":false,"message":sign.message,"target":null,"results":[]}))
                    .into_response()
            }
        }
    }
    let points = req
        .probes
        .iter()
        .take(3)
        .map(|p| core::GeoPoint {
            lat: p.latitude,
            lon: p.longitude,
        })
        .collect();
    let target = core::solve_radar_target(points, distances);
    if !target.ok {
        return Json(serde_json::json!({"ok":false,"message":target.message,"target":null,"results":[]}))
            .into_response();
    }
    let ids: Vec<(String, String)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    let jobs: Vec<Box<dyn FnOnce() -> SignResult + Send>> = accounts
        .into_iter()
        .map(|account| {
            let rid = rollcall_id.clone();
            let latitude = target.lat;
            let longitude = target.lon;
            let job: Box<dyn FnOnce() -> SignResult + Send> =
                Box::new(move || sign_radar_one(account, rid, latitude, longitude));
            job
        })
        .collect();
    let delays = stagger_plan(&settings, jobs.len());
    let results = run_batched(
        SIGN_BATCH_SIZE,
        Duration::from_secs(SIGN_JOB_TIMEOUT_SECONDS),
        None,
        delays,
        jobs,
    )
    .await;
    let mut out = Vec::new();
    let mut probes = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        let (id, name) = &ids[index];
        match result {
            Some(result) => {
                register_sign_probe(&mut probes, id, &result);
                out.push(sign_json(id, Some(name), &result, true));
            }
            None => out.push(sign_json(
                id,
                Some(name),
                &SignResult::fail(SIGN_TIMEOUT_HINT),
                true,
            )),
        }
    }
    apply_probes(&st, &probes);
    Json(serde_json::json!({
        "ok":true,"message":"已完成雷达签到",
        "target":{"latitude":target.lat,"longitude":target.lon},
        "results":out
    }))
    .into_response()
}

#[tokio::main]
async fn main() {
    let _ = env_required("ADMIN_PASSWORD");
    let mut store = load_store();
    // 历史状态里可能有空的/重复的 device_id，启动时收敛一次，否则签到会撞 device_used
    ensure_unique_device_ids(&mut store);
    let state = AppState::new(store);
    let proxy = Router::new()
        .route("/cas/:id/*target", axum::routing::any(cas_proxy))
        .route("/authserver/*target", axum::routing::any(cas_root_proxy))
        .route("/authserver", axum::routing::any(cas_root_proxy_index))
        .with_state(state.clone());
    let proxy_addr: SocketAddr = env::var("CAS_PROXY_BIND")
        .unwrap_or_else(|_| "0.0.0.0:1113".into())
        .parse()
        .unwrap();
    let proxy_listener = tokio::net::TcpListener::bind(proxy_addr).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(proxy_listener, proxy).await {
            eprintln!("CAS proxy listener stopped: {e}");
        }
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/admin/login", post(admin_login))
        .route("/api/session", get(session_status))
        .route("/api/cas/start", post(cas_start))
        .route("/api/cas/status/:id", get(cas_status))
        .route("/api/settings", get(settings_get).put(settings_put))
        .route("/api/accounts", get(accounts))
        .route("/api/accounts/:id", axum::routing::put(account_toggle).delete(account_delete))
        .route("/api/account-status", post(account_check))
        .route("/api/login", post(school_login))
        .route("/api/refresh", post(refresh))
        .route("/api/rollcalls", get(rollcalls))
        .route("/api/sign/qr", post(qr))
        .route("/api/sign/qr/account", post(qr_account))
        .route("/api/sign/number", post(number))
        .route("/api/sign/number/account", post(number_account))
        .route("/api/sign/radar", post(radar))
        .route("/api/sign/radar/account", post(radar_account))
        .route("/api/sign/radar/empty", post(radar_empty))
        .route("/api/sign/radar/empty/account", post(radar_empty_account))
        .route("/api/sign/all", post(sign_all))
        .route("/api/sign/radar/triangulate", post(radar_triangulate))
        .with_state(state);
    let addr: SocketAddr = env::var("BIND")
        .unwrap_or_else(|_| "0.0.0.0:1112".into())
        .parse()
        .unwrap();
    println!("winsignin web listening on {addr}, CAS proxy on {proxy_addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, student_id: &str) -> Account {
        Account {
            id: id.into(),
            username: "2023001".into(),
            name: "测试".into(),
            student_id: student_id.into(),
            cookie: "session=old".into(),
            cas_cookie: "cas".into(),
            bfp: "bfp".into(),
            device_id: "dev".into(),
            enabled: true,
            expired: false,
            checked_at: 0,
            status_detail: String::new(),
        }
    }

    /// 线上 /data/state.json 是旧格式（没有状态字段），升级后必须能直接读。
    #[test]
    fn legacy_state_without_status_fields_loads() {
        let json = r#"{"token":"t","accounts":[{"id":"a1","username":"2023001","name":"甲","student_id":"2023001","cookie":"session=x","cas_cookie":"","bfp":"","device_id":"d1","enabled":true}]}"#;
        let store: Store = serde_json::from_str(json).expect("旧状态文件应能读取");
        assert_eq!(store.accounts.len(), 1);
        assert!(!store.accounts[0].expired);
        assert_eq!(store.accounts[0].checked_at, 0);
        assert_eq!(store.settings.max_delay_seconds, 2);
    }

    #[test]
    fn relogin_updates_existing_account_and_drops_duplicates() {
        let mut store = Store::default();
        store.accounts.push(account("old", "2023001"));
        store.accounts.push(account("dup", "2023001"));
        let mut fresh = account("new", "2023001");
        fresh.cookie = "session=fresh".into();
        fresh.device_id = "other-device".into();
        fresh.enabled = false;
        let updated = upsert_account(&mut store, fresh);
        assert!(updated);
        assert_eq!(store.accounts.len(), 1, "同一学号的重复记录应合并");
        assert_eq!(store.accounts[0].id, "old");
        assert_eq!(store.accounts[0].cookie, "session=fresh", "新会话要写进去");
        assert_eq!(store.accounts[0].device_id, "dev", "设备标识保持不变");
        assert!(store.accounts[0].enabled, "重新登录不改用户的启用/停用选择");
    }

    #[test]
    fn probe_marks_and_clears_expiry() {
        let mut a = account("a", "2023001");
        apply_probe(&mut a, &SessionProbe::Expired("登录已过期，请重新登录".into()));
        assert!(a.expired);
        assert!(a.checked_at > 0);
        apply_probe(&mut a, &SessionProbe::Ok);
        assert!(!a.expired);
        assert!(a.status_detail.is_empty());
        apply_probe(&mut a, &SessionProbe::Failed("网络错误：连接超时".into()));
        assert!(!a.expired, "检测失败不应把账号判成过期");
        assert!(a.status_detail.contains("状态检测失败"));
    }

    /// 登录一次不能把别的标签页踢下线：旧 token 必须继续有效。
    #[test]
    fn relogin_keeps_previous_tokens_valid() {
        let mut store = Store::default();
        let first = store.issue_token();
        assert!(store.accept_token(&first));
        let second = store.issue_token();
        assert!(store.accept_token(&first), "旧会话不能因为再登录一次就失效");
        assert!(store.accept_token(&second));
        assert!(!store.accept_token(""));
        assert!(!store.accept_token("不是我们签发的 token"));
    }

    /// 但也不能无限增长：窗口之外的老 token 会被挤掉。
    #[test]
    fn token_window_is_bounded() {
        let mut store = Store::default();
        let mut issued = Vec::new();
        for _ in 0..(MAX_SESSION_TOKENS + 4) {
            issued.push(store.issue_token());
        }
        assert!(store.tokens.len() <= MAX_SESSION_TOKENS);
        assert!(store.accept_token(issued.last().unwrap()));
        assert!(
            !store.accept_token(&issued[0]),
            "最早的 token 应该已经被挤出窗口"
        );
    }

    /// 旧状态文件只有单个 token 字段，升级后它仍然要能登录。
    #[test]
    fn legacy_single_token_state_still_authenticates() {
        let store: Store = serde_json::from_str(r#"{"token":"legacy","accounts":[]}"#).unwrap();
        assert!(store.accept_token("legacy"));
        assert!(!store.accept_token("other"));
    }

    /// device_id 撞车会被上游判成 device_used，落实到每个账号必须唯一。
    #[test]
    fn duplicate_and_missing_device_ids_are_normalized() {
        let mut store = Store::default();
        let mut a = account("a", "2023001");
        a.device_id = "same".into();
        let mut b = account("b", "2023002");
        b.device_id = "same".into();
        let mut c = account("c", "2023003");
        c.device_id = "   ".into();
        store.accounts = vec![a, b, c];
        ensure_unique_device_ids(&mut store);
        let ids: Vec<String> = store.accounts.iter().map(|x| x.device_id.clone()).collect();
        assert_eq!(ids[0], "same", "第一个占住这个名字的保持不变");
        assert_ne!(ids[1], ids[0], "撞车的账号要换一个 device_id");
        assert!(!ids[2].trim().is_empty(), "空的 device_id 要补上");
        assert_ne!(ids[2], ids[0]);
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "规范化之后不应再有重复");
    }

    /// 重新登录保留账号自己的 device_id，并且不会把它和别的账号搞成一对。
    #[test]
    fn upsert_keeps_each_account_own_device_id() {
        let mut store = Store::default();
        let mut a = account("old", "2023001");
        a.device_id = "dup".into();
        let mut b = account("other", "2023002");
        b.device_id = "dup".into();
        store.accounts = vec![a, b];
        let mut fresh = account("new", "2023001");
        fresh.device_id = "brand-new".into();
        upsert_account(&mut store, fresh);
        let kept = store
            .accounts
            .iter()
            .find(|x| x.student_id == "2023001")
            .unwrap();
        assert_eq!(kept.device_id, "dup", "保留该账号历史 device_id");
        let other = store
            .accounts
            .iter()
            .find(|x| x.student_id == "2023002")
            .unwrap();
        assert_ne!(other.device_id, kept.device_id, "另一个账号必须换成唯一值");
    }

    #[test]
    fn upsert_assigns_device_id_when_new_account_has_none() {
        let mut store = Store::default();
        store.accounts.push(account("a", "2023001"));
        let mut fresh = account("", "2023999");
        fresh.device_id = String::new();
        assert!(!upsert_account(&mut store, fresh));
        let created = store
            .accounts
            .iter()
            .find(|x| x.student_id == "2023999")
            .unwrap();
        assert!(!created.device_id.trim().is_empty());
        assert_ne!(created.device_id, "dev");
    }

    /// 分批参数：不传就签所有启用的；传了就按顺序取，找不到的作为失败返回。
    #[test]
    fn select_accounts_respects_account_ids() {        let mut a = account("a", "2023001");
        let mut b = account("b", "2023002");
        b.enabled = false;
        let c = account("c", "2023003");
        a.enabled = true;
        let accounts = vec![a, b, c];

        let (picked, missing) = select_accounts(&accounts, None);
        assert_eq!(
            picked.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"],
            "不指定就只签启用的账号"
        );
        assert!(missing.is_empty());

        let ids = vec![
            "c".to_string(),
            "b".to_string(),
            "zzz".to_string(),
            "c".to_string(),
        ];
        let (picked, missing) = select_accounts(&accounts, Some(&ids));
        assert_eq!(
            picked.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["c", "b"],
            "按给定顺序取，停用的账号也能手动签到，重复的只留一次"
        );
        assert_eq!(missing.len(), 1, "不存在的账号要作为失败结果返回");
        assert_eq!(missing[0].0, "zzz");
    }

    #[test]
    fn stagger_plan_stops_waiting_after_budget() {
        let settings = Settings {
            min_delay_seconds: 20,
            max_delay_seconds: 20,
            auto_number: false,
            radar_scheme: "trilateration".into(),
            probes: Vec::new(),
        };
        let plan = stagger_plan(&settings, 5);
        assert_eq!(plan[0], Duration::ZERO, "第一个账号不等");
        let total: u64 = plan.iter().map(|d| d.as_secs()).sum();
        assert!(
            total <= STAGGER_BUDGET_SECONDS,
            "错峰累计不能超过软预算，否则一批请求会被反代读超时切断：{total}"
        );
    }

    #[test]
    fn number_code_plan_follows_settings() {
        assert!(matches!(
            resolve_number_code(" 1234 ", false),
            NumberCode::Explicit(code) if code == "1234"
        ));
        assert!(matches!(resolve_number_code("", true), NumberCode::Auto));
        assert!(matches!(resolve_number_code("   ", false), NumberCode::Missing));
    }

    #[test]
    fn sign_target_and_coordinate_validation() {
        assert!(validate_sign_target("a", "r").is_ok());
        assert!(validate_sign_target("", "r").is_err());
        assert!(validate_sign_target("a", " ").is_err());
        assert!(validate_coords(29.5, 106.5).is_ok());
        assert!(validate_coords(91.0, 106.5).is_err());
        assert!(validate_coords(29.5, -181.0).is_err());
        assert!(validate_coords(f64::NAN, 106.5).is_err());
        assert!(validate_coords(29.5, f64::INFINITY).is_err());
    }

    /// 签到结果要能透出上游判定，否则前端只能看到一句"签到失败"。
    #[test]
    fn sign_json_exposes_upstream_verdict() {
        let mut result = SignResult::fail("该设备已签到，请更换设备重新扫描");
        result.status = "device_used".into();
        result.http_code = 422;
        result.distance = Some(12.5);
        let value = sign_json("a1", Some("甲"), &result, true);
        assert_eq!(value["success"], serde_json::json!(false));
        assert_eq!(value["message"], serde_json::json!("该设备已签到，请更换设备重新扫描"));
        assert_eq!(value["upstream_status"], serde_json::json!("device_used"));
        assert_eq!(value["upstream_code"], serde_json::json!(422));
        assert_eq!(value["error_code"], serde_json::json!(""));
        assert_eq!(value["distance"], serde_json::json!(12.5));
        assert_eq!(value["name"], serde_json::json!("甲"));

        let with_error = SignResult::fail("不在签到范围内");
        let value = sign_error_json("a1", "account_not_found", &with_error.message, false);
        assert_eq!(value["error"], serde_json::json!("account_not_found"));
        assert_eq!(value["account_id"], serde_json::json!("a1"));
    }

    fn pending(id: &str, kind: RollcallKind, expired: bool, accounts: &[(&str, &str)]) -> PendingRollcall {
        PendingRollcall {
            rollcall_id: id.into(),
            course_title: format!("课程{id}"),
            title: "签到".into(),
            kind,
            is_expired: expired,
            accounts: accounts
                .iter()
                .map(|(aid, status)| ((*aid).to_string(), format!("名{aid}"), (*status).to_string()))
                .collect(),
        }
    }

    /// 跨课程：多门课同时点名时，每场只排"没签过的账号"，已签到/已结束的跳过。
    #[test]
    fn plan_all_signs_covers_every_course_but_skips_signed() {
        let mut grouped = HashMap::new();
        // 数学课：甲已签，乙没签
        grouped.insert(
            "rc-math".to_string(),
            pending("rc-math", RollcallKind::Number, false, &[("a", "on_call_fine"), ("b", "absent")]),
        );
        // 英语课：甲乙都没签
        grouped.insert(
            "rc-eng".to_string(),
            pending("rc-eng", RollcallKind::Radar, false, &[("a", "absent"), ("b", "absent")]),
        );
        // 已结束的课不该排任务
        grouped.insert(
            "rc-old".to_string(),
            pending("rc-old", RollcallKind::Qr, true, &[("a", "absent")]),
        );

        let (jobs, skipped, truncated) = plan_all_signs(&grouped, None, 100);
        assert!(!truncated);
        let pairs: Vec<(String, String)> = jobs
            .iter()
            .map(|j| (j.rollcall_id.clone(), j.account_id.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("rc-eng".to_string(), "a".to_string()),
                ("rc-eng".to_string(), "b".to_string()),
                ("rc-math".to_string(), "b".to_string()),
            ],
            "跨课程全覆盖，但已签到的甲不该被数学课重复提交"
        );
        assert_eq!(skipped.len(), 1, "已结束的点名要报跳过原因");
        assert_eq!(skipped[0]["rollcall_id"], serde_json::json!("rc-old"));
        assert_eq!(skipped[0]["reason"], serde_json::json!("已结束"));
    }

    /// 全员已签到的点名要明确报"所有账号都已签到"，而不是静默消失。
    #[test]
    fn plan_all_signs_reports_fully_signed_rollcall() {
        let mut grouped = HashMap::new();
        grouped.insert(
            "rc1".to_string(),
            pending("rc1", RollcallKind::Number, false, &[("a", "on_call_fine")]),
        );
        let (jobs, skipped, _) = plan_all_signs(&grouped, None, 100);
        assert!(jobs.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["reason"], serde_json::json!("所有账号都已签到"));
    }

    /// rollcall_ids 过滤：只处理指定的课程。
    #[test]
    fn plan_all_signs_honours_rollcall_filter() {
        let mut grouped = HashMap::new();
        grouped.insert("rc1".to_string(), pending("rc1", RollcallKind::Number, false, &[("a", "absent")]));
        grouped.insert("rc2".to_string(), pending("rc2", RollcallKind::Number, false, &[("a", "absent")]));
        let only = vec!["rc2".to_string()];
        let (jobs, _, _) = plan_all_signs(&grouped, Some(&only), 100);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].rollcall_id, "rc2");
    }

    /// 任务数上限：超出要标记 truncated，不能静默丢。
    #[test]
    fn plan_all_signs_reports_truncation() {
        let mut grouped = HashMap::new();
        grouped.insert(
            "rc1".to_string(),
            pending("rc1", RollcallKind::Number, false, &[("a", "absent"), ("b", "absent"), ("c", "absent")]),
        );
        let (jobs, _, truncated) = plan_all_signs(&grouped, None, 2);
        assert_eq!(jobs.len(), 2);
        assert!(truncated, "超过上限必须回报 truncated");
    }

    /// 已签到状态的判定：只认 on_call_fine（与 verify_signed / AstrBot 插件一致）。
    #[test]
    fn signed_status_detection() {
        assert!(status_is_signed("on_call_fine"));
        assert!(status_is_signed(" on_call_fine "));
        assert!(
            !status_is_signed("on_call"),
            "on_call 不能当成已签到，否则会漏签"
        );
        assert!(!status_is_signed("absent"));
        assert!(!status_is_signed(""));
    }

    /// 一个账号同时有 3 门课要签时，必须产出 3 个任务（不能因为按账号取用就只剩 1 个）。
    #[test]
    fn plan_all_signs_keeps_every_course_for_one_account() {
        let mut grouped = HashMap::new();
        for id in ["rc-a", "rc-b", "rc-c"] {
            grouped.insert(
                id.to_string(),
                pending(id, RollcallKind::Number, false, &[("a", "absent")]),
            );
        }
        let (jobs, _, _) = plan_all_signs(&grouped, None, 100);
        assert_eq!(jobs.len(), 3, "同一账号的 3 门课都要排上");
        assert!(jobs.iter().all(|j| j.account_id == "a"));
    }

    /// rollcall_ids 传空数组与不传等价（都表示全部），和 account_ids 的语义对齐。
    #[test]
    fn plan_all_signs_treats_empty_filter_as_all() {
        let mut grouped = HashMap::new();
        grouped.insert("rc1".to_string(), pending("rc1", RollcallKind::Number, false, &[("a", "absent")]));
        let empty: Vec<String> = Vec::new();
        let (jobs, _, _) = plan_all_signs(&grouped, Some(&empty), 100);
        assert_eq!(jobs.len(), 1, "空数组应表示全部，而不是一个都不选");
    }

    /// 位置类点名没给坐标时，用探测点中心兜底；没有探测点就返回 None。
    #[test]
    fn probe_center_falls_back_to_saved_probes() {
        assert!(probe_center(&[]).is_none());
        let probes = vec![
            RadarProbe { latitude: 29.0, longitude: 106.0 },
            RadarProbe { latitude: 29.2, longitude: 106.2 },
        ];
        let (lat, lon) = probe_center(&probes).expect("两个点应能算出中心");
        assert!((lat - 29.1).abs() < 1e-9);
        assert!((lon - 106.1).abs() < 1e-9);
        // 越界探测点不应产出可用坐标
        let bad = vec![RadarProbe { latitude: 200.0, longitude: 106.0 }];
        assert!(probe_center(&bad).is_none());
    }
}
