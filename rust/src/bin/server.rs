use axum::{extract::{Path, Query, State}, http::{header, HeaderMap, HeaderValue, StatusCode}, response::{Html, IntoResponse, Response}, routing::{get, post}, Json, Router};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, fs::{self, OpenOptions}, io::Write, net::SocketAddr, path::PathBuf, sync::{Arc, Mutex}, time::{Duration, SystemTime, UNIX_EPOCH}};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use winsignin::{self as core, AccountInfo, LoginResult, RollcallsResult, SignResult};

#[derive(Clone)]
struct AppState { inner: Arc<Mutex<Store>> }
#[derive(Serialize, Deserialize, Default)]
struct Store { token: String, accounts: Vec<Account>, #[serde(default)] settings: Settings }
#[derive(Serialize, Deserialize, Clone)]
struct Settings { min_delay_seconds: u64, max_delay_seconds: u64, auto_number: bool, radar_scheme: String, probes: Vec<RadarProbe> }
impl Default for Settings { fn default() -> Self { Self { min_delay_seconds: 0, max_delay_seconds: 1, auto_number: false, radar_scheme: "trilateration".into(), probes: Vec::new() } } }
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
#[derive(Deserialize)] struct AdminLogin { password: String }
#[derive(Deserialize)] struct SettingsInput { min_delay_seconds: u64, max_delay_seconds: u64, auto_number: bool, radar_scheme: String, probes: Vec<RadarProbe> }
#[derive(Deserialize)] struct SchoolLogin { username: String, password: String, bfp: Option<String>, remember_me: Option<bool>, cas_cookie: Option<String> }
#[derive(Deserialize)] struct QrReq { raw: String }
#[derive(Deserialize)] struct NumberReq { rollcall_id: String, code: String }
#[derive(Deserialize)] struct RadarReq { rollcall_id: String, latitude: f64, longitude: f64 }
#[derive(Serialize, Deserialize, Clone)] struct RadarProbe { latitude: f64, longitude: f64 }
#[derive(Deserialize)] struct RadarTriangulateReq { rollcall_id: String, probes: Vec<RadarProbe> }

fn data_path() -> PathBuf { PathBuf::from(env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())).join("state.json") }
fn load_store() -> Store { let p=data_path(); match fs::read(&p) { Ok(bytes)=>{restrict_state_permissions(&p); serde_json::from_slice(&bytes).expect("状态文件损坏，拒绝启动以免覆盖账号数据")}, Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Store::default(), Err(e)=>panic!("读取状态文件失败：{e}") } }
#[cfg(unix)]
fn restrict_state_permissions(path: &PathBuf) { fs::set_permissions(path,fs::Permissions::from_mode(0o600)).expect("设置状态文件权限失败"); }
#[cfg(not(unix))]
fn restrict_state_permissions(_path: &PathBuf) {}
fn create_private_file(path: &PathBuf) -> fs::File { let mut options=OpenOptions::new(); options.write(true).create_new(true); #[cfg(unix)] options.mode(0o600); options.open(path).expect("创建状态临时文件失败") }
fn save_store(s: &Store) { let p=data_path(); let parent=p.parent().expect("DATA_DIR 必须有父目录"); fs::create_dir_all(parent).expect("创建数据目录失败"); let tmp=p.with_file_name(format!("state-{}.tmp",new_token())); let bytes=serde_json::to_vec_pretty(s).expect("序列化状态失败"); let mut f=create_private_file(&tmp); f.write_all(&bytes).expect("写入状态失败"); f.sync_all().expect("同步状态失败"); fs::rename(&tmp,&p).expect("原子替换状态文件失败"); }
fn new_token() -> String { rand::thread_rng().sample_iter(&Alphanumeric).take(48).map(char::from).collect() }
fn env_required(k: &str) -> String { env::var(k).unwrap_or_else(|_| panic!("必须设置环境变量 {k}")) }
fn origin_allowed(headers: &HeaderMap) -> bool { let origin=headers.get(header::ORIGIN).and_then(|v|v.to_str().ok()); let expected=env::var("MAIN_ORIGIN").unwrap_or_else(|_|"https://sign.xiaohundun.cc".into()); origin.map_or(true,|o|o==expected.as_str()) }
fn auth(headers: &HeaderMap, s: &Store) -> bool { if !origin_allowed(headers) { return false; } let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or(""); let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or(""); bearer == format!("Bearer {}", s.token) || cookie.split(';').any(|v| v.trim() == format!("session={}", s.token)) }
fn authorized(st: &AppState, headers: &HeaderMap) -> bool { let s=st.inner.lock().unwrap(); auth(headers,&s) }
fn authorized_accounts(st: &AppState, headers: &HeaderMap) -> Result<Vec<Account>,Response> { let s=st.inner.lock().unwrap(); if !auth(headers,&s) { Err(unauthorized()) } else { Ok(s.accounts.clone()) } }
fn rollcalls_api_token_auth(headers: &HeaderMap) -> bool { if !origin_allowed(headers) { return false; } let expected=env::var("WINSIGNIN_API_TOKEN").unwrap_or_default(); if expected.is_empty() { return false; } let bearer=headers.get(header::AUTHORIZATION).and_then(|v|v.to_str().ok()).unwrap_or(""); bearer==format!("Bearer {expected}") }
fn authorized_rollcall_accounts(st: &AppState, headers: &HeaderMap) -> Result<Vec<Account>,Response> { let s=st.inner.lock().unwrap(); if auth(headers,&s) || rollcalls_api_token_auth(headers) { Ok(s.accounts.clone()) } else { Err(unauthorized()) } }
fn authorized_signing_data(st: &AppState, headers: &HeaderMap) -> Result<(Vec<Account>,Settings),Response> { let s=st.inner.lock().unwrap(); if !auth(headers,&s) { Err(unauthorized()) } else { Ok((s.accounts.iter().filter(|a|a.enabled).cloned().collect(),s.settings.clone())) } }
fn unauthorized() -> Response { (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"ok":false,"message":"未登录"}))).into_response() }
fn json_result(ok: bool, message: impl Into<String>) -> Json<serde_json::Value> { Json(serde_json::json!({"ok":ok,"message":message.into()})) }
fn stagger_seconds(settings: &Settings, index: usize) -> u64 { if index == 0 || settings.max_delay_seconds == 0 { 0 } else { rand::thread_rng().gen_range(settings.min_delay_seconds..=settings.max_delay_seconds) } }
async fn stagger(settings: &Settings, index: usize) { let secs=stagger_seconds(settings,index); if secs>0 { tokio::time::sleep(Duration::from_secs(secs)).await; } }

const SESSION_EXPIRED_HINT: &str = "登录已过期";
fn unix_now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d|d.as_secs()).unwrap_or(0) }
fn is_session_expired(message: &str) -> bool { message.contains(SESSION_EXPIRED_HINT) }

/// 一次探测得到的会话结论，只用于更新缓存的账号状态。
enum SessionProbe { Ok, Expired(String), Failed(String) }

fn probe_from_account(info: AccountInfo) -> SessionProbe {
    if info.ok { SessionProbe::Ok }
    else if is_session_expired(&info.message) { SessionProbe::Expired(info.message) }
    else { SessionProbe::Failed(info.message) }
}

fn status_ok(a: &mut Account) { a.checked_at=unix_now(); a.expired=false; a.status_detail.clear(); }
fn status_expired(a: &mut Account, message: &str) { a.checked_at=unix_now(); a.expired=true; a.status_detail=message.to_string(); }
fn status_check_failed(a: &mut Account, message: &str) { a.checked_at=unix_now(); a.status_detail=format!("状态检测失败：{message}"); }
fn apply_probe(a: &mut Account, probe: &SessionProbe) {
    match probe {
        SessionProbe::Ok => status_ok(a),
        SessionProbe::Expired(message) => status_expired(a,message),
        SessionProbe::Failed(message) => status_check_failed(a,message),
    }
}

/// 把一批探测结论写回账号；只有状态真的变了才落盘，避免轮询把状态文件写个不停。
fn apply_probes(st: &AppState, probes: &[(String, SessionProbe)]) {
    if probes.is_empty() { return; }
    let mut s=st.inner.lock().unwrap();
    let mut changed=false;
    for (id,probe) in probes {
        let Some(a)=s.accounts.iter_mut().find(|a|a.id==*id) else { continue };
        let before=(a.expired,a.status_detail.clone());
        apply_probe(a,probe);
        if (a.expired,a.status_detail.clone())!=before { changed=true; }
    }
    if changed { save_store(&s); }
}

fn account_json(a: &Account) -> serde_json::Value {
    serde_json::json!({
        "id":a.id,"username":a.username,"name":a.name,"student_id":a.student_id,
        "enabled":a.enabled,"expired":a.expired,"checked_at":a.checked_at,
        "status_detail":a.status_detail,"can_refresh":!a.cas_cookie.trim().is_empty(),
    })
}
fn accounts_json(s: &Store) -> Vec<serde_json::Value> { s.accounts.iter().map(account_json).collect() }

/// 签到结果也能反映会话状态：成功说明会话有效，提示"登录已过期"就标成过期。
fn register_sign_probe(probes: &mut Vec<(String, SessionProbe)>, id: &str, r: &SignResult) {
    if r.success { probes.push((id.to_string(),SessionProbe::Ok)); }
    else if is_session_expired(&r.message) { probes.push((id.to_string(),SessionProbe::Expired(r.message.clone()))); }
}

/// 同一学号只保留一条账号：重新登录时更新原账号的会话，保留原本的 id、启用状态、
/// 指纹和 deviceId，并清掉历史遗留的重复记录。返回是否更新了已有账号。
fn upsert_account(s: &mut Store, mut account: Account) -> bool {
    if account.student_id.is_empty() { s.accounts.push(account); return false; }
    let mut positions: Vec<usize> = s.accounts.iter().enumerate().filter(|(_,a)|a.student_id==account.student_id).map(|(i,_)|i).collect();
    let Some(first)=positions.first().copied() else { s.accounts.push(account); return false; };
    let old_id=s.accounts[first].id.clone();
    let old_enabled=s.accounts[first].enabled;
    let old_device=s.accounts[first].device_id.clone();
    let old_bfp=s.accounts[first].bfp.clone();
    account.id=old_id;
    account.enabled=old_enabled;
    account.device_id=old_device;
    if account.bfp.is_empty() { account.bfp=old_bfp; }
    s.accounts[first]=account;
    for index in positions.drain(1..).rev() { s.accounts.remove(index); }
    true
}

async fn index() -> Html<&'static str> { Html(include_str!("../../../web/index.html")) }
async fn admin_login(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminLogin>) -> Response {
    if !origin_allowed(&headers) { return (StatusCode::FORBIDDEN,json_result(false,"Origin 不匹配")).into_response(); }
    if req.password != env_required("ADMIN_PASSWORD") { return (StatusCode::UNAUTHORIZED, json_result(false, "管理口令错误")).into_response(); }
    let mut s = st.inner.lock().unwrap(); s.token = new_token(); save_store(&s);
    let mut r = json_result(true, "").into_response(); r.headers_mut().insert(header::SET_COOKIE, format!("session={}; HttpOnly; Secure; SameSite=Strict; Path=/", s.token).parse().unwrap()); r
}
async fn accounts(State(st): State<AppState>, headers: HeaderMap) -> Response { let s=st.inner.lock().unwrap(); if !auth(&headers,&s){return unauthorized();} Json(accounts_json(&s)).into_response() }
/// 主动探测每个账号的会话是否还有效（请求学校 /api/profile），结果缓存进 state.json，
/// 网页端据此显示「正常 / 已过期」。
async fn account_check(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let list=match authorized_accounts(&st,&headers){Ok(x)=>x,Err(r)=>return r};
    let handles: Vec<_>=list.into_iter().map(|a| tokio::task::spawn_blocking(move||{ let info=core::get_account(a.cookie); (a.id,info) })).collect();
    let mut probes=Vec::new();
    for handle in handles {
        match handle.await { Ok((id,info))=>probes.push((id,probe_from_account(info))), Err(e)=>eprintln!("账号状态检测任务失败：{e}") }
    }
    apply_probes(&st,&probes);
    let s=st.inner.lock().unwrap();
    Json(serde_json::json!({"ok":true,"accounts":accounts_json(&s)})).into_response()
}
async fn account_toggle(State(st): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Response { let mut s=st.inner.lock().unwrap(); if !auth(&headers,&s){return unauthorized();} match s.accounts.iter_mut().find(|a|a.id==id) { Some(a)=>{a.enabled=!a.enabled; let enabled=a.enabled; save_store(&s); Json(serde_json::json!({"ok":true,"enabled":enabled})).into_response()}, None=>(StatusCode::NOT_FOUND,json_result(false,"账号不存在")).into_response() } }
async fn account_delete(State(st): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Response { let mut s=st.inner.lock().unwrap(); if !auth(&headers,&s){return unauthorized();} let n=s.accounts.len(); s.accounts.retain(|a|a.id!=id); if s.accounts.len()==n{return (StatusCode::NOT_FOUND,json_result(false,"账号不存在")).into_response()} save_store(&s); json_result(true,"账号已删除").into_response() }
async fn school_login(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<SchoolLogin>) -> Response {
    if !authorized(&st,&headers){return unauthorized();}
    let username = req.username;
    let bfp = req.bfp.unwrap_or_default();
    let password = req.password;
    let remember = req.remember_me.unwrap_or(true);
    let cas_cookie = req.cas_cookie.unwrap_or_default();
    let r: LoginResult = tokio::task::spawn_blocking({ let u=username.clone(); let b=bfp.clone(); move || core::login(u, password, b, remember, cas_cookie) }).await.unwrap();
    if !r.ok { return (StatusCode::BAD_REQUEST, json_result(false, r.message)).into_response(); }
    let info: AccountInfo = tokio::task::spawn_blocking({let c=r.cookie.clone(); move || core::get_account(c)}).await.unwrap();
    let mut account=Account{id:new_token()[..12].to_string(), username, name:info.name.clone(), student_id:info.id.clone(), cookie:r.cookie, cas_cookie:r.cas_cookie, bfp, device_id:new_token()[..16].to_string(), enabled:true, expired:false, checked_at:unix_now(), status_detail:String::new()};
    match probe_from_account(info) {
        SessionProbe::Ok => {}
        SessionProbe::Expired(message) => { account.expired=true; account.status_detail=message; }
        SessionProbe::Failed(message) => { account.status_detail=format!("状态检测失败：{message}"); }
    }
    let mut s=st.inner.lock().unwrap();
    let updated=upsert_account(&mut s, account);
    save_store(&s);
    json_result(true, if updated {"登录成功，已更新原账号会话"} else {"登录成功"}).into_response()
}
async fn settings_get(State(st): State<AppState>, headers: HeaderMap) -> Response { let s=st.inner.lock().unwrap(); if !auth(&headers,&s){return unauthorized();} Json(&s.settings).into_response() }
async fn settings_put(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<SettingsInput>) -> Response { let mut s=st.inner.lock().unwrap(); if !auth(&headers,&s){return unauthorized();} if req.min_delay_seconds>60 || req.max_delay_seconds>60 || req.min_delay_seconds>req.max_delay_seconds || req.probes.len()>20 || req.probes.iter().any(|p| !(-90.0..=90.0).contains(&p.latitude) || !(-180.0..=180.0).contains(&p.longitude)) || !matches!(req.radar_scheme.as_str(),"empty"|"trilateration") { return (StatusCode::BAD_REQUEST,json_result(false,"设置范围无效")).into_response(); } s.settings=Settings{min_delay_seconds:req.min_delay_seconds,max_delay_seconds:req.max_delay_seconds,auto_number:req.auto_number,radar_scheme:req.radar_scheme,probes:req.probes}; save_store(&s); json_result(true,"设置已保存").into_response() }
async fn session_status(State(st): State<AppState>, headers: HeaderMap) -> Response { let s=st.inner.lock().unwrap(); if !auth(&headers,&s){return unauthorized();} json_result(true,"已登录").into_response() }
async fn cas_start(State(st): State<AppState>, headers: HeaderMap) -> Response { if !authorized(&st,&headers){return unauthorized();} match core::browser_start().await { Ok(flow)=>{let base=env::var("CAS_PROXY_PUBLIC_URL").unwrap_or_else(|_|"https://cqupt.xiaohundun.cc".into()); Json(serde_json::json!({"ok":true,"flow_id":flow.id,"url":format!("{}{}",base.trim_end_matches('/'),flow.url)})).into_response()}, Err(e)=>(StatusCode::BAD_GATEWAY,json_result(false,e)).into_response() } }
async fn cas_status(State(st): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Response { if !authorized(&st,&headers){return unauthorized();} let Some(done)=core::browser_take_completed(&id) else {return (StatusCode::ACCEPTED,json_result(false,"等待完成学校认证")).into_response()}; let cookie=done.cookie.clone(); let info=tokio::task::spawn_blocking(move||core::get_account(cookie)).await.unwrap(); if !info.ok {return (StatusCode::BAD_GATEWAY,json_result(false,info.message)).into_response()} let account=Account{id:new_token()[..12].to_string(),username:info.id.clone(),name:info.name,student_id:info.id,cookie:done.cookie,cas_cookie:done.cas_cookie,bfp:String::new(),device_id:new_token()[..16].to_string(),enabled:true,expired:false,checked_at:unix_now(),status_detail:String::new()}; let mut s=st.inner.lock().unwrap(); let updated=upsert_account(&mut s,account); save_store(&s); Json(serde_json::json!({"ok":true,"message":if updated {"学校网页登录成功，已更新原账号会话"} else {"学校网页登录成功，账号已保存"}})).into_response() }
async fn cas_root_proxy(Path(target): Path<String>, headers: HeaderMap, request: axum::http::Request<axum::body::Body>) -> Response { let flow=headers.get(header::COOKIE).and_then(|v|v.to_str().ok()).unwrap_or("").split(';').filter_map(|p|p.trim().strip_prefix("casflow=")).next().unwrap_or("").to_string(); if flow.is_empty(){return (StatusCode::NOT_FOUND,json_result(false,"认证代理会话缺失，请重新打开认证页面")).into_response()} cas_proxy_forward(flow,"ids.cqupt.edu.cn",format!("/authserver/{target}"),request).await }
async fn cas_root_proxy_index(headers: HeaderMap, request: axum::http::Request<axum::body::Body>) -> Response { let flow=headers.get(header::COOKIE).and_then(|v|v.to_str().ok()).unwrap_or("").split(';').filter_map(|p|p.trim().strip_prefix("casflow=")).next().unwrap_or("").to_string(); if flow.is_empty(){return (StatusCode::NOT_FOUND,json_result(false,"认证代理会话缺失")).into_response()} cas_proxy_forward(flow,"ids.cqupt.edu.cn","/authserver".into(),request).await }
async fn cas_proxy(Path((id,target)): Path<(String,String)>, request: axum::http::Request<axum::body::Body>) -> Response { let Some((host,rest))=target.split_once('/') else {return (StatusCode::BAD_REQUEST,json_result(false,"代理路径错误")).into_response()}; cas_proxy_forward(id,host,format!("/{rest}"),request).await }
async fn cas_proxy_forward(id:String,host:&str,path:String,request:axum::http::Request<axum::body::Body>) -> Response { let query=request.uri().query().map(str::to_string); let method=request.method().as_str().to_string(); let content_type=request.headers().get(header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).map(str::to_string); let accept=request.headers().get(header::ACCEPT).and_then(|v|v.to_str().ok()).map(str::to_string); let requested_with=request.headers().get("x-requested-with").and_then(|v|v.to_str().ok()).map(str::to_string); let body=match axum::body::to_bytes(request.into_body(),4*1024*1024).await {Ok(b)=>b.to_vec(),Err(_)=>return (StatusCode::PAYLOAD_TOO_LARGE,json_result(false,"认证请求过大")).into_response()}; let upstream=match core::browser_forward(&id,host,&path,query.as_deref(),&method,body,content_type.as_deref(),accept.as_deref(),requested_with.as_deref()).await {Ok(x)=>x,Err(e)=>return (StatusCode::BAD_GATEWAY,json_result(false,e)).into_response()}; let mut builder=Response::builder().status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY)); if let Some(ct)=upstream.content_type { builder=builder.header(header::CONTENT_TYPE,ct); } if let Some(loc)=upstream.location { builder=builder.header(header::LOCATION,loc); } let main_origin=env::var("MAIN_ORIGIN").unwrap_or_else(|_|"https://sign.xiaohundun.cc".into()); builder=builder.header("Content-Security-Policy",format!("frame-ancestors {main_origin}")).header("X-Content-Type-Options","nosniff"); if path.contains("/authserver/login") || path.contains("/authserver/") {builder=builder.header(header::SET_COOKIE,format!("casflow={id}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=600"));} builder.body(axum::body::Body::from(upstream.body)).unwrap_or_else(|_|StatusCode::BAD_GATEWAY.into_response()) }
async fn health() -> Json<serde_json::Value> { Json(serde_json::json!({"ok":true,"service":"winsignin"})) }
/// 用保存的统一认证票据静默刷新会话，逐个账号汇报结果：
/// 成功的更新 cookie，失败的说明原因（通常是 14 天的 TGT 到期，需要重新输密码）。
async fn refresh(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let list=match authorized_accounts(&st,&headers){Ok(x)=>x,Err(r)=>return r};
    let mut updated=0usize;
    let mut failed=Vec::new();
    let mut skipped=Vec::new();
    for a in list {
        let id=a.id.clone();
        let name=a.name.clone();
        let username=a.username.clone();
        if a.cas_cookie.trim().is_empty() {
            skipped.push(serde_json::json!({"id":id,"name":name,"username":username,"message":"没有保存统一认证票据，请用帐号密码重新登录"}));
            continue;
        }
        let r=tokio::task::spawn_blocking({let c=a.cas_cookie.clone(); move||core::refresh_session(c)}).await.unwrap();
        if !r.ok {
            // 刷新失败多为统一认证 TGT 到期；旧的 lms 会话可能还没失效，顺手探测真实状态。
            let info=tokio::task::spawn_blocking({let c=a.cookie.clone(); move||core::get_account(c)}).await.unwrap();
            let probe=probe_from_account(info);
            let mut s=st.inner.lock().unwrap();
            if let Some(x)=s.accounts.iter_mut().find(|x|x.id==id) { apply_probe(x,&probe); }
            save_store(&s);
            failed.push(serde_json::json!({"id":id,"name":name,"username":username,"message":r.message}));
            continue;
        }
        let info=tokio::task::spawn_blocking({let c=r.cookie.clone(); move||core::get_account(c)}).await.unwrap();
        let probe=probe_from_account(info);
        let mut s=st.inner.lock().unwrap();
        if let Some(x)=s.accounts.iter_mut().find(|x|x.id==id) {
            x.cookie=r.cookie;
            x.cas_cookie=r.cas_cookie;
            apply_probe(x,&probe);
            updated+=1;
        }
        save_store(&s);
    }
    let s=st.inner.lock().unwrap();
    Json(serde_json::json!({"ok":true,"updated":updated,"failed":failed,"skipped":skipped,"accounts":accounts_json(&s)})).into_response()
}

#[derive(Deserialize, Default)]
struct RollcallsQuery { monitor: Option<String> }
impl RollcallsQuery {
    fn monitor_enabled(&self) -> bool { matches!(self.monitor.as_deref(), Some("1" | "true")) }
}

#[derive(Serialize)]
struct AccountHealth { account_id: String, name: String, state: &'static str, message: &'static str }
fn account_health(account_id: &str, name: &str, probe: &SessionProbe) -> AccountHealth {
    // Never expose upstream errors or session details through the read-only monitor API.
    let (state,message)=match probe {
        SessionProbe::Ok => ("ok","账号状态正常"),
        SessionProbe::Expired(_) => ("expired","登录已过期，请重新登录"),
        SessionProbe::Failed(_) => ("failed","暂时无法获取点名，请稍后重试"),
    };
    AccountHealth { account_id:account_id.into(), name:name.into(), state, message }
}

async fn rollcalls(State(st): State<AppState>, Query(query): Query<RollcallsQuery>, headers: HeaderMap) -> Response {
    let list=match authorized_rollcall_accounts(&st,&headers){Ok(x)=>x,Err(r)=>return r};
    let mut grouped:HashMap<String,serde_json::Value>=HashMap::new();
    // Start every account before awaiting any result, so request timeouts do not add up serially.
    let handles:Vec<_>=list.into_iter().filter(|a|a.enabled).map(|a| {
        let account_id=a.id;
        let account_name=a.name;
        let handle=tokio::task::spawn_blocking(move||core::get_rollcalls(a.cookie));
        (account_id,account_name,handle)
    }).collect();
    let accounts_checked=handles.len();
    let mut complete=accounts_checked>0;
    let mut probes=Vec::new();
    let mut health=Vec::new();
    for (account_id,account_name,handle) in handles {
        let r:RollcallsResult=match handle.await {
            Ok(r)=>r,
            Err(_)=>RollcallsResult { ok:false, message:"点名查询任务失败".into(), rollcalls:Vec::new() },
        };
        let probe=if r.ok { SessionProbe::Ok }
            else if is_session_expired(&r.message) { SessionProbe::Expired(r.message.clone()) }
            else { SessionProbe::Failed(r.message.clone()) };
        health.push(account_health(&account_id,&account_name,&probe));
        probes.push((account_id.clone(),probe));
        if !r.ok {
            complete=false;
            continue;
        }
        for x in r.rollcalls {
            let rid=x.id.clone();
            let entry=grouped.entry(rid.clone()).or_insert_with(||serde_json::json!({"id":rid,"course_title":x.course_title,"title":x.title,"is_number":x.is_number,"is_radar":x.is_radar,"is_expired":x.is_expired,"status":x.status,"rollcall_time":x.rollcall_time,"accounts":[]}));
            if x.is_expired { entry["is_expired"] = serde_json::Value::Bool(true); }
            if let Some(accounts)=entry.get_mut("accounts").and_then(|v|v.as_array_mut()) {
                accounts.push(serde_json::json!({"account_id":account_id,"name":account_name,"status":x.status}));
            }
        }
    }
    apply_probes(&st,&probes);
    let rollcalls=grouped.into_values().collect::<Vec<_>>();
    let mut response=if query.monitor_enabled() {
        Json(serde_json::json!({"rollcalls":rollcalls,"complete":complete,"account_health":health})).into_response()
    } else {
        Json(rollcalls).into_response()
    };
    response.headers_mut().insert("X-Rollcalls-Complete",HeaderValue::from_static(if complete {"true"} else {"false"}));
    response
}
async fn qr(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<QrReq>) -> Response { let raw=req.raw; let (accounts,settings)=match authorized_signing_data(&st,&headers){Ok(x)=>x,Err(r)=>return r}; let mut out=Vec::new(); let mut probes=Vec::new(); for (i,a) in accounts.into_iter().enumerate() { stagger(&settings,i).await; let id=a.id.clone(); let raw=raw.clone(); let r:SignResult=tokio::task::spawn_blocking(move||core::sign_qr(a.cookie,a.device_id,raw)).await.unwrap(); register_sign_probe(&mut probes,&id,&r); out.push(serde_json::json!({"account_id":id,"success":r.success,"message":r.message,"distance":r.distance})); } apply_probes(&st,&probes); Json(out).into_response() }
async fn number(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<NumberReq>) -> Response { let (accounts,settings)=match authorized_signing_data(&st,&headers){Ok(x)=>x,Err(r)=>return r}; let mut out=Vec::new(); let mut probes=Vec::new(); for (i,a) in accounts.into_iter().enumerate() { stagger(&settings,i).await; let aid=a.id.clone(); let rid=req.rollcall_id.clone(); let code=req.code.clone(); let auto=settings.auto_number; let r=tokio::task::spawn_blocking(move||{let code=if code.is_empty()&&auto {let x=core::get_number_code(a.cookie.clone(),rid.clone()); if !x.ok {return SignResult{success:false,message:x.message,distance:None}} x.code} else {code}; core::sign_number(a.cookie,a.device_id,rid,code)}).await.unwrap(); register_sign_probe(&mut probes,&aid,&r); out.push(serde_json::json!({"account_id":aid,"success":r.success,"message":r.message,"distance":r.distance})); } apply_probes(&st,&probes); Json(out).into_response() }
async fn radar_empty(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<NumberReq>) -> Response { let (accounts,settings)=match authorized_signing_data(&st,&headers){Ok(x)=>x,Err(r)=>return r}; let mut out=Vec::new(); let mut probes=Vec::new(); for (i,a) in accounts.into_iter().enumerate() { stagger(&settings,i).await; let aid=a.id.clone(); let rid=req.rollcall_id.clone(); let cookie=a.cookie.clone(); let r=tokio::task::spawn_blocking(move||{let sign=core::sign_radar_empty(cookie.clone(),rid.clone()); if sign.success {let v=core::verify_signed(cookie,rid); SignResult{success:v.ok,message:v.message,distance:None}} else {sign}}).await.unwrap(); register_sign_probe(&mut probes,&aid,&r); out.push(serde_json::json!({"account_id":aid,"success":r.success,"message":r.message})); } apply_probes(&st,&probes); Json(out).into_response() }
async fn radar(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RadarReq>) -> Response { let (accounts,settings)=match authorized_signing_data(&st,&headers){Ok(x)=>x,Err(r)=>return r}; let mut out=Vec::new(); let mut probes=Vec::new(); for (i,a) in accounts.into_iter().enumerate() { stagger(&settings,i).await; let aid=a.id.clone(); let rid=req.rollcall_id.clone(); let lat=req.latitude; let lon=req.longitude; let r=tokio::task::spawn_blocking(move||{let cookie=a.cookie.clone(); let sign=core::sign_radar(cookie.clone(),a.device_id,rid.clone(),lat,lon); if sign.success {let v=core::verify_signed(cookie,rid); SignResult{success:v.ok,message:v.message,distance:None}} else {sign}}).await.unwrap(); register_sign_probe(&mut probes,&aid,&r); out.push(serde_json::json!({"account_id":aid,"success":r.success,"message":r.message,"distance":r.distance})); } apply_probes(&st,&probes); Json(out).into_response() }

async fn radar_triangulate(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RadarTriangulateReq>) -> Response {
    if req.probes.len() < 3 || req.probes.iter().take(3).any(|p| !(-90.0..=90.0).contains(&p.latitude) || !(-180.0..=180.0).contains(&p.longitude)) { return (StatusCode::BAD_REQUEST, json_result(false, "至少需要 3 个有效探测点")).into_response(); }
    let (accounts,settings)=match authorized_signing_data(&st,&headers){Ok(x)=>x,Err(r)=>return r};
    let probe_account=match accounts.first().cloned(){Some(a)=>a,None=>return (StatusCode::BAD_REQUEST,json_result(false,"没有启用的账号")).into_response()};
    let mut distances=Vec::new();
    for p in req.probes.iter().take(3) { let c=probe_account.cookie.clone(); let d=probe_account.device_id.clone(); let rid=req.rollcall_id.clone(); let lat=p.latitude; let lon=p.longitude; let r=tokio::task::spawn_blocking(move||core::sign_radar(c,d,rid,lat,lon)).await.unwrap(); if r.success { let v=core::verify_signed(probe_account.cookie.clone(),req.rollcall_id.clone()); return Json(serde_json::json!({"ok":v.ok,"message":v.message,"target":null,"results":[]})).into_response(); } match r.distance { Some(x)=>distances.push(x), None=>return Json(serde_json::json!({"ok":false,"message":r.message,"target":null,"results":[]})).into_response() } }
    let points=req.probes.iter().take(3).map(|p|core::GeoPoint{lat:p.latitude,lon:p.longitude}).collect(); let target=core::solve_radar_target(points,distances); if !target.ok { return Json(serde_json::json!({"ok":false,"message":target.message,"target":null,"results":[]})).into_response(); }
    let mut results=Vec::new(); let mut probes=Vec::new(); for (i,a) in accounts.into_iter().enumerate() { stagger(&settings,i).await; let aid=a.id.clone(); let rid=req.rollcall_id.clone(); let lat=target.lat; let lon=target.lon; let r=tokio::task::spawn_blocking(move||{let cookie=a.cookie.clone(); let sign=core::sign_radar(cookie.clone(),a.device_id,rid.clone(),lat,lon); if sign.success { let v=core::verify_signed(cookie,rid); SignResult{success:v.ok,message:v.message,distance:None} } else { sign }}).await.unwrap(); register_sign_probe(&mut probes,&aid,&r); results.push(serde_json::json!({"account_id":aid,"success":r.success,"message":r.message,"distance":r.distance})); }
    apply_probes(&st,&probes);
    Json(serde_json::json!({"ok":true,"message":"已完成雷达签到","target":{"latitude":target.lat,"longitude":target.lon},"results":results})).into_response()
}

#[tokio::main]
async fn main() { let _=env_required("ADMIN_PASSWORD"); let state=AppState{inner:Arc::new(Mutex::new(load_store()))}; let proxy=Router::new().route("/cas/:id/*target",axum::routing::any(cas_proxy)).route("/authserver/*target",axum::routing::any(cas_root_proxy)).route("/authserver",axum::routing::any(cas_root_proxy_index)).with_state(state.clone()); let proxy_addr:SocketAddr=env::var("CAS_PROXY_BIND").unwrap_or_else(|_|"0.0.0.0:1113".into()).parse().unwrap(); let proxy_listener=tokio::net::TcpListener::bind(proxy_addr).await.unwrap(); tokio::spawn(async move { if let Err(e)=axum::serve(proxy_listener,proxy).await { eprintln!("CAS proxy listener stopped: {e}"); } }); let app=Router::new().route("/",get(index)).route("/health",get(health)).route("/api/admin/login",post(admin_login)).route("/api/session",get(session_status)).route("/api/cas/start",post(cas_start)).route("/api/cas/status/:id",get(cas_status)).route("/api/settings",get(settings_get).put(settings_put)).route("/api/accounts",get(accounts)).route("/api/accounts/:id",axum::routing::put(account_toggle).delete(account_delete)).route("/api/account-status",post(account_check)).route("/api/login",post(school_login)).route("/api/refresh",post(refresh)).route("/api/rollcalls",get(rollcalls)).route("/api/sign/qr",post(qr)).route("/api/sign/number",post(number)).route("/api/sign/radar",post(radar)).route("/api/sign/radar/empty",post(radar_empty)).route("/api/sign/radar/triangulate",post(radar_triangulate)).with_state(state); let addr:SocketAddr=env::var("BIND").unwrap_or_else(|_|"0.0.0.0:1112".into()).parse().unwrap(); println!("winsignin web listening on {addr}, CAS proxy on {proxy_addr}"); axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(),app).await.unwrap(); }

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, student_id: &str) -> Account {
        Account { id:id.into(), username:"2023001".into(), name:"测试".into(), student_id:student_id.into(), cookie:"session=old".into(), cas_cookie:"cas".into(), bfp:"bfp".into(), device_id:"dev".into(), enabled:true, expired:false, checked_at:0, status_detail:String::new() }
    }

    /// 线上 /data/state.json 是旧格式（没有状态字段），升级后必须能直接读。
    #[test]
    fn legacy_state_without_status_fields_loads() {
        let json = r#"{"token":"t","accounts":[{"id":"a1","username":"2023001","name":"甲","student_id":"2023001","cookie":"session=x","cas_cookie":"","bfp":"","device_id":"d1","enabled":true}]}"#;
        let store: Store = serde_json::from_str(json).expect("旧状态文件应能读取");
        assert_eq!(store.accounts.len(),1);
        assert!(!store.accounts[0].expired);
        assert_eq!(store.accounts[0].checked_at,0);
        assert_eq!(store.settings.max_delay_seconds,1);
    }

    #[test]
    fn relogin_updates_existing_account_and_drops_duplicates() {
        let mut store=Store::default();
        store.accounts.push(account("old","2023001"));
        store.accounts.push(account("dup","2023001"));
        let mut fresh=account("new","2023001");
        fresh.cookie="session=fresh".into();
        fresh.device_id="other-device".into();
        fresh.enabled=false;
        let updated=upsert_account(&mut store, fresh);
        assert!(updated);
        assert_eq!(store.accounts.len(),1,"同一学号的重复记录应合并");
        assert_eq!(store.accounts[0].id,"old");
        assert_eq!(store.accounts[0].cookie,"session=fresh","新会话要写进去");
        assert_eq!(store.accounts[0].device_id,"dev","设备标识保持不变");
        assert!(store.accounts[0].enabled,"重新登录不改用户的启用/停用选择");
    }

    #[test]
    fn probe_marks_and_clears_expiry() {
        let mut a=account("a","2023001");
        apply_probe(&mut a,&SessionProbe::Expired("登录已过期，请重新登录".into()));
        assert!(a.expired);
        assert!(a.checked_at>0);
        apply_probe(&mut a,&SessionProbe::Ok);
        assert!(!a.expired);
        assert!(a.status_detail.is_empty());
        apply_probe(&mut a,&SessionProbe::Failed("网络错误：连接超时".into()));
        assert!(!a.expired,"检测失败不应把账号判成过期");
        assert!(a.status_detail.contains("状态检测失败"));
    }
}
