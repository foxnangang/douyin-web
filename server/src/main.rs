//! 抖音解析网页版后端。
//! 复用 douyin-downloader 的 a_bogus 签名与接口层，对外提供 HTTP JSON API。
//! 前端静态资源由 public/ 目录提供，Nginx 可反代本服务。

mod db;
mod douyin;
mod error;
mod models;
mod paths;
mod zipstore;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{body::Body, Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

use db::Db;
use douyin::parser::{parse_all, Owner};
use douyin::DouyinClient;
use error::AppError;
use models::{FetchMode, ProfileInfo, TaskRow};

// ---------- 请求 / 响应结构 ----------

#[derive(Debug, Deserialize)]
struct ParseReq {
    url: String,
    #[serde(default)]
    cookie: String,
}

#[derive(Debug, Deserialize)]
struct FetchReq {
    #[serde(default)]
    url: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    cookie: String,
    /// 最多返回多少条，0 表示不限。防止误操作把服务器打爆。
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_mode() -> String {
    "single".into()
}
fn default_limit() -> usize {
    200
}

#[derive(Debug, Deserialize)]
struct CookieReq {
    cookie: String,
}

#[derive(Debug, Deserialize)]
struct DownloadReq {
    url: String,
    /// 下载保存的文件名（可选）
    #[serde(default)]
    name: String,
    /// 1 = 浏览器内打开（inline），默认下载
    #[serde(default)]
    inline: u8,
}

#[derive(Serialize)]
struct ApiResp<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl<T> ApiResp<T> {
    fn ok(data: T) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, data: None, error: Some(msg.into()) }
    }
}

#[derive(Serialize)]
struct ParseData {
    profile: ProfileInfo,
    works: Vec<TaskRow>,
    count: usize,
    mode: String,
}

#[derive(Serialize)]
struct CookieData {
    valid: bool,
    profile: Option<ProfileInfo>,
}

/// 把抖音原始 JSON 转成前端可直接消费的扁平作品行。
fn to_rows(profile: &ProfileInfo, list: &[serde_json::Value]) -> Vec<TaskRow> {
    parse_all(list, &Owner::from_profile(profile))
}

#[derive(Clone)]
struct AppState {
    client: Arc<DouyinClient>,
    db: Arc<Db>,
    /// 内网发码口令（博客付款成功后回调用），空值则关闭该接口
    internal_token: String,
    /// 未登录试用：ip -> (日期, 当日已用次数)
    trial: Arc<Mutex<HashMap<String, (String, u32)>>>,
    /// 后台会话：token -> 过期时间戳（秒）。内存态，重启失效。
    admin_sessions: Arc<Mutex<HashMap<String, i64>>>,
    /// 后台登录失败计数：ip -> (首次失败时间戳, 次数)
    admin_fails: Arc<Mutex<HashMap<String, (i64, u32)>>>,
}

/// 未登录每日可免费解析的次数
const TRIAL_PER_DAY: u32 = 2;

/// 各操作扣的额度（按真实资源消耗定，ASR 最贵）
fn quota_cost(kind: &str) -> i64 {
    match kind {
        "parse" => 1,    // 单链接解析
        "fetch" => 5,    // 主页/点赞批量，一次几百条
        "asr" => 3,      // 语音转文字，占 CPU 20~30 秒
        "ai" => 1,       // 大模型改写
        _ => 1,
    }
}

fn mode_from(s: &str) -> Result<FetchMode, AppError> {
    FetchMode::from_slug(s)
        .ok_or_else(|| AppError::msg(format!("未知模式: {s}，可用 single/post/favorite/collect")))
}

// ---------- 账号 / 会话 / 额度 ----------

const COOKIE_NAME: &str = "dy_sid";

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix("dy_sid=") {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn client_ip(headers: &HeaderMap) -> String {
    for key in ["x-real-ip", "x-forwarded-for"] {
        if let Some(v) = headers.get(key).and_then(|v| v.to_str().ok()) {
            let first = v.split(',').next().unwrap_or("").trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    "unknown".to_string()
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

fn session_cookie(token: &str, secure: bool) -> String {
    let mut s = format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000");
    if secure {
        s.push_str("; Secure");
    }
    s
}

fn clear_cookie(secure: bool) -> String {
    let mut s = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        s.push_str("; Secure");
    }
    s
}

fn json_err(code: StatusCode, msg: &str) -> Response {
    (
        code,
        Json(serde_json::json!({ "ok": false, "error": msg })),
    )
        .into_response()
}

/// 只返回一句人话，由调用方决定 HTTP 状态码。
/// （被封禁和未登录要分开提示，否则用户会一直去登录）
fn require_user(st: &AppState, headers: &HeaderMap) -> Result<db::User, String> {
    let Some(tok) = cookie_token(headers) else {
        return Err("请先登录后再用（右上角登录 / 注册）".into());
    };
    match st.db.user_by_token(&tok) {
        Some(u) if u.status == "active" => Ok(u),
        Some(_) => Err("账号已被停用，请联系管理员".into()),
        None => Err("登录已过期，请重新登录".into()),
    }
}

/// 开工前只「查」额度，不扣。
/// 之前是先扣再干活，解析失败也照样扣，用户会骂人。
fn ensure_quota(st: &AppState, headers: &HeaderMap, kind: &str) -> Result<db::User, String> {
    let user = require_user(st, headers)?;
    let cost = quota_cost(kind);
    let bal = st.db.balance(user.id);
    if bal < cost {
        return Err(format!(
            "额度不足（剩余 {bal} 次）。去右上角充一个邀请码继续用"
        ));
    }
    Ok(user)
}

/// 真正扣费：只在业务成功后调用。
fn settle(st: &AppState, user: &db::User, kind: &str) {
    let cost = quota_cost(kind);
    match st.db.charge(user.id, kind, cost) {
        Ok(Ok(after)) => tracing::info!("user {} -{cost} {kind} -> {after}", user.id),
        Ok(Err(bal)) => tracing::warn!("user {} 余额不足({bal})，{kind} 未扣", user.id),
        Err(e) => tracing::warn!("扣额度失败: {}", e.text()),
    }
}

/// 未登录试用：只「查」不消耗，成功后由 trial_consume 记账。
fn trial_available(st: &AppState, headers: &HeaderMap) -> bool {
    let ip = client_ip(headers);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut map = st.trial.lock().unwrap();
    let entry = map.entry(ip).or_insert((today.clone(), 0));
    if entry.0 != today {
        *entry = (today, 0);
    }
    entry.1 < TRIAL_PER_DAY
}

fn trial_consume(st: &AppState, headers: &HeaderMap) {
    let ip = client_ip(headers);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut map = st.trial.lock().unwrap();
    let entry = map.entry(ip).or_insert((today.clone(), 0));
    if entry.0 != today {
        *entry = (today, 0);
    }
    entry.1 += 1;
}

fn hash_password(pw: &str) -> Result<String, String> {
    // 自己取 16 字节随机盐再交给 argon2：
    // password-hash 的 OsRng 被 getrandom feature 门控，这里不额外开 feature。
    use rand::RngCore;
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| format!("盐生成失败：{e}"))?;
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("密码处理失败：{e}"))
}

fn verify_password(pw: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ---------- 账号接口 ----------

#[derive(Debug, Deserialize)]
struct RegisterReq {
    code: String,
    email: String,
    password: String,
}

async fn auth_register(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterReq>,
) -> Response {
    let email = req.email.trim().to_lowercase();
    let code = req.code.trim().to_uppercase();
    if !db::valid_email(&email) {
        return json_err(StatusCode::BAD_REQUEST, "邮箱格式不对");
    }
    if req.password.len() < 6 {
        return json_err(StatusCode::BAD_REQUEST, "密码至少 6 位");
    }
    if code.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "请填邀请码");
    }

    let hash = match hash_password(&req.password) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };

    match st.db.register(&email, &hash, &code) {
        Ok(user) => {
            let token = db::gen_token();
            if let Err(e) = st.db.create_session(user.id, &token, &client_ip(&headers)) {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.text());
            }
            let cookie = session_cookie(&token, is_https(&headers));
            let mut h = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                h.insert(header::SET_COOKIE, v);
            }
            (
                h,
                Json(serde_json::json!({
                    "ok": true,
                    "data": { "email": user.email, "balance": user.balance }
                })),
            )
                .into_response()
        }
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.text()),
    }
}

#[derive(Debug, Deserialize)]
struct LoginReq {
    email: String,
    password: String,
}

async fn auth_login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Response {
    let email = req.email.trim().to_lowercase();
    let Some((id, hash, status, balance, nickname)) = st.db.find_login(&email) else {
        return json_err(StatusCode::UNAUTHORIZED, "邮箱或密码不对");
    };
    if status != "active" {
        return json_err(StatusCode::FORBIDDEN, "账号已被停用");
    }
    if !verify_password(&req.password, &hash) {
        return json_err(StatusCode::UNAUTHORIZED, "邮箱或密码不对");
    }
    let token = db::gen_token();
    if let Err(e) = st.db.create_session(id, &token, &client_ip(&headers)) {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.text());
    }
    let cookie = session_cookie(&token, is_https(&headers));
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        h.insert(header::SET_COOKIE, v);
    }
    (
        h,
        Json(serde_json::json!({
            "ok": true,
            "data": { "email": email, "nickname": nickname, "balance": balance }
        })),
    )
        .into_response()
}

async fn auth_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(tok) = cookie_token(&headers) {
        st.db.drop_session(&tok);
    }
    let cookie = clear_cookie(is_https(&headers));
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        h.insert(header::SET_COOKIE, v);
    }
    (h, Json(serde_json::json!({ "ok": true }))).into_response()
}

async fn auth_me(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match require_user(&st, &headers) {
        Ok(u) => Json(serde_json::json!({
            "ok": true,
            "data": { "email": u.email, "nickname": u.nickname, "balance": u.balance }
        }))
        .into_response(),
        Err(_) => Json(serde_json::json!({ "ok": false, "data": null })).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct RedeemReq {
    code: String,
}

/// 已登录用户充额度（老用户买新码）。
async fn auth_redeem(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RedeemReq>,
) -> Response {
    let user = match require_user(&st, &headers) {
        Ok(u) => u,
        Err(msg) => return json_err(StatusCode::UNAUTHORIZED, &msg),
    };
    let code = req.code.trim().to_uppercase();
    if code.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "请填邀请码");
    }
    match st.db.redeem(user.id, &code) {
        Ok(balance) => Json(serde_json::json!({
            "ok": true,
            "data": { "email": user.email, "balance": balance }
        }))
        .into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.text()),
    }
}

#[derive(Debug, Deserialize)]
struct NickReq {
    nickname: String,
}

/// 用户自己改昵称：1~16 字，去掉首尾空白
async fn auth_nickname(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NickReq>,
) -> Response {
    let user = match require_user(&st, &headers) {
        Ok(u) => u,
        Err(msg) => return json_err(StatusCode::UNAUTHORIZED, &msg),
    };
    let name: String = req.nickname.trim().chars().take(16).collect();
    if name.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "昵称不能为空");
    }
    match st.db.set_nickname(user.id, &name) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "data": { "email": user.email, "nickname": name, "balance": user.balance }
        }))
        .into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.text()),
    }
}

// ---------- 内网发码（博客付款成功后调用） ----------

#[derive(Debug, Deserialize)]
struct IssueReq {
    #[serde(default)]
    quota: Option<i64>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    order_id: Option<String>,
    #[serde(default)]
    expire_days: Option<i64>,
}

/// 只有带对内网口令才可用；同一 order_id 重复调用返回同一个码（幂等）。
async fn issue_code_internal(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IssueReq>,
) -> Response {
    let got = headers
        .get("x-internal-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if st.internal_token.is_empty() || got != st.internal_token {
        return json_err(StatusCode::FORBIDDEN, "forbidden");
    }

    let order_id = req.order_id.unwrap_or_default();
    if !order_id.is_empty() {
        if let Some(existing) = st.db.code_by_order(&order_id) {
            return Json(serde_json::json!({
                "ok": true,
                "data": { "code": existing, "reused": true }
            }))
            .into_response();
        }
    }

    let quota = req.quota.unwrap_or(100).clamp(1, 100_000);
    let note = req.note.unwrap_or_default();
    let expire_days = req.expire_days.unwrap_or(365).clamp(0, 3650);

    match st.db.issue_code(quota, &note, &order_id, expire_days) {
        Ok(code) => Json(serde_json::json!({
            "ok": true,
            "data": { "code": code, "quota": quota, "reused": false }
        }))
        .into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.text()),
    }
}

// ---------- 处理函数 ----------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true, "service": "douyin-web", "version": "1.0.0" }))
}

/// 单链接解析：返回作品直链与元数据（无水印源地址）。
async fn parse(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ParseReq>,
) -> Json<ApiResp<ParseData>> {
    let user = match ensure_quota(&st, &headers, "parse") {
        Ok(u) => u,
        Err(msg) => return Json(ApiResp::err(msg)),
    };
    let url = pick_url(&req.url);
    let url = url.as_str();
    if url.is_empty() {
        return Json(ApiResp::err("请粘贴作品链接"));
    }
    match st.client.fetch_plain(url, &req.cookie, FetchMode::Single).await {
        Ok((profile, list)) => {
            let works = to_rows(&profile, &list);
            let count = works.len();
            settle(&st, &user, "parse");
            Json(ApiResp::ok(ParseData {
                profile,
                works,
                count,
                mode: "single".into(),
            }))
        }
        Err(e) => Json(ApiResp::err(e.to_string())),
    }
}

/// 批量抓取：主页 / 点赞 / 收藏。
async fn fetch(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<FetchReq>,
) -> Json<ApiResp<ParseData>> {
    let user = match ensure_quota(&st, &headers, "fetch") {
        Ok(u) => u,
        Err(msg) => return Json(ApiResp::err(msg)),
    };
    let mode = match mode_from(&req.mode) {
        Ok(m) => m,
        Err(e) => return Json(ApiResp::err(e.to_string())),
    };
    if mode == FetchMode::Single {
        return Json(ApiResp::err("批量抓取请用 post/favorite/collect，单作品请走 /api/parse"));
    }
    let status = st.client.check_cookie(&req.cookie).await;
    if status != models::CookieStatus::Ok {
        return Json(ApiResp::err("Cookie 无效或已过期，请重新粘贴"));
    }
    let share_url = pick_url(&req.url);
    match st.client.fetch_plain(&share_url, &req.cookie, mode).await {
        Ok((profile, mut list)) => {
            if req.limit > 0 && list.len() > req.limit {
                list.truncate(req.limit);
            }
            let works = to_rows(&profile, &list);
            let count = works.len();
            settle(&st, &user, "fetch");
            Json(ApiResp::ok(ParseData {
                profile,
                works,
                count,
                mode: req.mode,
            }))
        }
        Err(e) => Json(ApiResp::err(e.to_string())),
    }
}

/// 校验 Cookie 并返回账号信息。
async fn check_cookie(
    State(st): State<AppState>,
    Json(req): Json<CookieReq>,
) -> Json<ApiResp<CookieData>> {
    let cookie = req.cookie.trim();
    if cookie.is_empty() {
        return Json(ApiResp::err("请粘贴 Cookie"));
    }
    let status = st.client.check_cookie(cookie).await;
    if status != models::CookieStatus::Ok {
        return Json(ApiResp::ok(CookieData { valid: false, profile: None }));
    }
    match st.client.self_profile(cookie).await {
        Ok(p) => Json(ApiResp::ok(CookieData { valid: true, profile: Some(p) })),
        Err(e) => Json(ApiResp::err(e.to_string())),
    }
}

/// 抖音 CDN 允许的域名后缀。白名单防止此接口被当成开放代理滥用。
#[allow(dead_code)]
const CDN_ALLOW: &[&str] = &[
    "douyinvod.com",
    "douyin.com",
    "douyinpic.com",
    "ixigua.com",
    "365yg.com",
    "byteimg.com",
    "snssdk.com",
    "bytedance.com",
    "byte-cdn.com",
    "bytecdn.cn",
    "tiktok.com",
    "cdn-ali.tiktok.com",
];

#[allow(dead_code)]
fn is_allowed_cdn(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if u.scheme() != "https" && u.scheme() != "http" {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    CDN_ALLOW.iter().any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// 允许代取的公网地址判定。
///
/// 多平台（快手 / 小红书 / B 站 / 微博…）的 CDN 域名太多太长尾，维护一份精确白名单
/// 不现实，改成「只挡内网」：拒绝裸 IP、localhost、常见内网后缀与非 80/443 端口。
/// 这样既能代理各家 CDN，又不至于被当成内网探测入口。
fn is_public_url(raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw) else {
        return false;
    };
    if u.scheme() != "https" && u.scheme() != "http" {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    let h = host.to_ascii_lowercase();

    // 裸 IP 一律拒绝：CDN 都是域名，走 IP 的多半是探测
    if h.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    if h == "localhost"
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".localhost")
    {
        return false;
    }
    // 必须有域名点，杜绝单段主机名
    if !h.contains('.') {
        return false;
    }
    matches!(u.port(), None | Some(80) | Some(443))
}

/// 从用户粘贴的整段分享文案里挑出链接。
///
/// 抖音分享出来的是「4.64 04/02 q@r.Eh :1pm rEH/ 一只真土鸡…# 土鸡 # 散养鸡 https://v.douyin.com/xxx/」
/// 这种带一堆说明和话题的文本，直接当 URL 解析必然失败，得先截出 http 开头那一段。
fn pick_url(raw: &str) -> String {
    let t = raw.trim();
    // 一律靠正则截：纯链接后面也可能紧贴中文（「https://v.douyin.com/xxx/复制此链接」）
    let re = Regex::new(r#"https?://[^\s，,。；;、）)】」”\"']+"#).unwrap();
    match re.find(t) {
        Some(m) => m
            .as_str()
            .trim_end_matches(|c: char| "，,。、；;）)】」”\"'".contains(c))
            .to_string(),
        // 没有链接就原样返回，让后面的校验给出提示
        None => t.to_string(),
    }
}

/// 取 CDN 资源：先不带 Referer 试一次，失败再补抖音 Referer 重试。
///
/// 各平台 CDN 的防盗链策略不一样——字节系要抖音 Referer，快手/小红书/B站 反而
/// 对陌生 Referer 更敏感。两段式比维护一张「哪家要什么 Referer」的表可靠。
async fn fetch_media(
    client: &reqwest::Client,
    url: &str,
    range: Option<HeaderValue>,
) -> Result<reqwest::Response, String> {
    let mut first = client.get(url).header(header::USER_AGENT, douyin::api::UA);
    if let Some(rg) = range.clone() {
        first = first.header(header::RANGE, rg);
    }
    let resp = first
        .send()
        .await
        .map_err(|e| format!("请求 CDN 失败: {e}"))?;
    if resp.status().is_success() {
        return Ok(resp);
    }

    let mut second = client
        .get(url)
        .header(header::USER_AGENT, douyin::api::UA)
        .header(header::REFERER, "https://www.douyin.com/");
    if let Some(rg) = range {
        second = second.header(header::RANGE, rg);
    }
    second
        .send()
        .await
        .map_err(|e| format!("请求 CDN 失败: {e}"))
}

/// 流式代理下载 CDN 文件。
/// 浏览器直连 CDN 会因 Referer 校验返回 403；由后端带正确 Referer 转发即可。
///
/// 转发客户端的 `Range`：`<video>` 播放时浏览器会按段请求，不转发就只能整段下载，
/// 表现为进度条无法拖动。
async fn download(
    State(st): State<AppState>,
    Query(q): Query<DownloadReq>,
    headers_in: HeaderMap,
) -> Response {
    if !is_public_url(&q.url) {
        return (StatusCode::BAD_REQUEST, "仅允许代理公网 CDN 地址").into_response();
    }

    let range = headers_in.get(header::RANGE).cloned();
    let upstream = match fetch_media(&st.client.client, &q.url, range).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
    };

    let status = upstream.status();
    if !status.is_success() {
        return (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("CDN 返回 {status}（可能需要有效 Cookie 或链接已过期）"),
        )
            .into_response();
    }

    // 透传内容类型与长度，并强制浏览器下载
    let mut headers = HeaderMap::new();
    if let Some(ct) = upstream.headers().get(header::CONTENT_TYPE) {
        headers.insert(header::CONTENT_TYPE, ct.clone());
    }
    if let Some(len) = upstream.headers().get(header::CONTENT_LENGTH) {
        headers.insert(header::CONTENT_LENGTH, len.clone());
    }
    // 206 分段响应：必须把 Content-Range 带回去，否则 <video> 会判定分段失败
    if let Some(cr) = upstream.headers().get(header::CONTENT_RANGE) {
        headers.insert(header::CONTENT_RANGE, cr.clone());
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    let name = if q.name.trim().is_empty() {
        "douyin-media".to_string()
    } else {
        q.name.trim().to_string()
    };
    let disposition_type = if q.inline == 1 { "inline" } else { "attachment" };
    let disposition = format!("{disposition_type}; filename=\"{name}\"; filename*=UTF-8''{}",
        percent_encoding::utf8_percent_encode(&name, percent_encoding::CONTROLS));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }

    let stream = upstream.bytes_stream();
    (status, headers, Body::from_stream(stream)).into_response()
}

#[derive(Debug, Deserialize)]
struct ImgReq {
    url: String,
}

/// 缩略图代理。
///
/// 抖音图片 CDN（`douyinpic.com` 等）对 `Referer` 有校验，浏览器里 `<img>`
/// 直接引用常年 403；即便放行，跨域 + 无缓存也慢。这里由后端带上正确
/// Referer 取回字节再吐给浏览器，并加长缓存头——用户反复翻列表不再回源。
async fn image_proxy(
    State(st): State<AppState>,
    Query(q): Query<ImgReq>,
) -> Response {
    if !is_public_url(&q.url) {
        return (StatusCode::BAD_REQUEST, "仅允许代理公网 CDN 地址").into_response();
    }

    let upstream = match fetch_media(&st.client.client, &q.url, None).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
    };

    let status = upstream.status();
    if !status.is_success() {
        return (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("图片 CDN 返回 {status}"),
        )
            .into_response();
    }

    // 只放行图片类型，避免这条路径被拿来传任意文件
    let ct = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    if !ct.starts_with("image/") {
        return (StatusCode::BAD_GATEWAY, format!("上游不是图片: {ct}")).into_response();
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("读取图片失败: {e}")).into_response()
        }
    };

    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&ct) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );

    (StatusCode::OK, headers, Body::from(bytes.to_vec())).into_response()
}

#[derive(Debug, Deserialize)]
struct QuickReq {
    url: String,
}

/// 免 Cookie 快速解析：本机 Python 解析服务（parse-video-py）转发。
/// 只接受抖音/主流平台分享短链，结果映射成前端统一的作品结构。
async fn quick_parse(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QuickReq>,
) -> Json<ApiResp<serde_json::Value>> {
    // 登录用户成功后扣额度；未登录给每天 TRIAL_PER_DAY 次试用
    let anonymous = cookie_token(&headers).is_none();
    let payer: Option<db::User> = match ensure_quota(&st, &headers, "parse") {
        Ok(u) => Some(u),
        Err(msg) => {
            if !(anonymous && trial_available(&st, &headers)) {
                return Json(ApiResp::err(msg));
            }
            None
        }
    };
    let share_url = pick_url(&q.url);
    if share_url.is_empty() {
        return Json(ApiResp::err("请粘贴分享链接"));
    }
    let target = format!(
        "http://127.0.0.1:8095/parse?url={}",
        percent_encoding::utf8_percent_encode(&share_url, percent_encoding::CONTROLS)
    );
    let resp = st
        .client
        .client
        .get(&target)
        .timeout(std::time::Duration::from_secs(45))
        .send()
        .await;
    let upstream = match resp {
        Ok(r) => r,
        Err(e) => return Json(ApiResp::err(format!("解析服务不可用: {e}"))),
    };
    let body: serde_json::Value = match upstream.json().await {
        Ok(v) => v,
        Err(e) => return Json(ApiResp::err(format!("解析服务响应异常: {e}"))),
    };
    if body.get("code").and_then(|v| v.as_i64()) != Some(200) {
        let msg = body.get("msg").and_then(|v| v.as_str()).unwrap_or("解析失败");
        return Json(ApiResp::err(msg));
    }
    let d = body.get("data").cloned().unwrap_or_default();
    let str_of = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_str()).unwrap_or("").to_string();

    let title = str_of(d.get("title"));
    let cover = str_of(d.get("cover_url"));
    let video_url = str_of(d.get("video_url"));
    let music_url = str_of(d.get("music_url"));
    let author_name = str_of(d.get("author").and_then(|a| a.get("name")));
    let author_avatar = str_of(d.get("author").and_then(|a| a.get("avatar")));
    let platform = str_of(d.get("platform"));
    let platform_label = str_of(d.get("platform_label"));

    let mut works: Vec<serde_json::Value> = Vec::new();

    // 视频（图集作品也可能带 livephoto 视频，两者都收）
    if !video_url.is_empty() {
        works.push(serde_json::json!({
            "id": "quick-video",
            "awemeId": "quick",
            "desc": title,
            "kind": "video",
            "url": video_url,
            "playUrl": video_url,
            "ext": "mp4",
            "isImage": false,
            "cover": cover,
            "date": "",
            "mix": "",
            "ownerNickname": author_name,
            "urlHash": "quick-video",
        }));
    }

    // 图集：每张图一行，前端可逐张下载或整包打包
    if let Some(images) = d.get("images").and_then(|v| v.as_array()) {
        for (i, img) in images.iter().enumerate() {
            let url = img.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let live = img
                .get("live_photo_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if url.is_empty() && live.is_empty() {
                continue;
            }
            let (kind, real, is_image, ext) = if live.is_empty() {
                ("image", url.clone(), true, "jpg")
            } else {
                ("live", live.clone(), true, "mp4")
            };
            works.push(serde_json::json!({
                "id": format!("quick-img-{i}"),
                "awemeId": "quick",
                "desc": format!("{title}_p{}", i + 1),
                "kind": kind,
                "url": real,
                "playUrl": real,
                "ext": ext,
                "isImage": is_image,
                "cover": if url.is_empty() { cover.clone() } else { url },
                "date": "",
                "mix": "",
                "ownerNickname": author_name,
                "urlHash": format!("quick-img-{i}"),
            }));
        }
    }

    if works.is_empty() {
        return Json(ApiResp::err("没有解析到可下载的内容，可能是链接已失效或该平台暂时不可用"));
    }
    let count = works.len();
    match &payer {
        Some(u) => settle(&st, u, "parse"),
        None => trial_consume(&st, &headers),
    }

    let profile = serde_json::json!({
        "nickname": author_name,
        "uniqueId": "",
        "avatarUrl": author_avatar,
        "secUid": "",
    });
    Json(ApiResp::ok(serde_json::json!({
        "profile": profile,
        "works": works,
        "count": count,
        "mode": "quick",
        "coverUrl": cover,
        "musicUrl": music_url,
        "platform": platform,
        "platformLabel": platform_label,
    })))
}

// ---------- 图集打包 ----------

#[derive(Debug, Deserialize)]
struct ZipReq {
    #[serde(default)]
    name: String,
    urls: Vec<String>,
}

/// 单次打包上限：张数与总字节。图集一般 9~18 张，6000 万像素图也不会到 120MB。
const ZIP_MAX_ITEMS: usize = 60;
const ZIP_MAX_BYTES: u64 = 120 * 1024 * 1024;

/// 由 URL 或 Content-Type 猜图片扩展名。
fn image_ext(url: &str, ct: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    if lower.contains(".png") {
        "png"
    } else if lower.contains(".webp") {
        "webp"
    } else if lower.contains(".gif") {
        "gif"
    } else if ct.contains("png") {
        "png"
    } else if ct.contains("webp") {
        "webp"
    } else if ct.contains("gif") {
        "gif"
    } else {
        "jpg"
    }
}

/// 图集打包下载：后端逐张取回，打成不压缩的 zip 一次性返回。
async fn zip_download(
    State(st): State<AppState>,
    Json(req): Json<ZipReq>,
) -> Response {
    let urls: Vec<String> = req
        .urls
        .into_iter()
        .filter(|u| is_public_url(u))
        .take(ZIP_MAX_ITEMS)
        .collect();
    if urls.is_empty() {
        return (StatusCode::BAD_REQUEST, "没有可打包的图片").into_response();
    }

    let base = {
        let n = req.name.trim();
        let n = if n.is_empty() { "images" } else { n };
        paths::sanitize_filename(n, 60)
    };

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    for (i, u) in urls.iter().enumerate() {
        let Ok(resp) = fetch_media(&st.client.client, u, None).await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let Ok(bytes) = resp.bytes().await else { continue };
        if bytes.is_empty() {
            continue;
        }
        total += bytes.len() as u64;
        if total > ZIP_MAX_BYTES {
            break;
        }
        let ext = image_ext(u, &ct);
        files.push((format!("{:02}.{}", i + 1, ext), bytes.to_vec()));
    }

    if files.is_empty() {
        return (StatusCode::BAD_GATEWAY, "图片全部获取失败，链接可能已过期").into_response();
    }

    let n = files.len();
    let zip = zipstore::build_zip(&files);
    tracing::info!("zip {} 张 -> {} 字节", n, zip.len());

    let fname = format!("{base}_{n}张.zip");
    let disposition = format!(
        "attachment; filename=\"album.zip\"; filename*=UTF-8''{}",
        percent_encoding::utf8_percent_encode(&fname, percent_encoding::CONTROLS)
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    (StatusCode::OK, headers, Body::from(zip)).into_response()
}

// ---------- 配音（TTS） ----------

#[derive(Debug, Deserialize)]
struct TtsReq {
    text: String,
    #[serde(default)]
    voice: String,
    #[serde(default)]
    rate: String,
}

/// 配音：转发到本机 Python 服务的 edge-tts。
async fn tts(State(st): State<AppState>, Query(q): Query<TtsReq>) -> Response {
    if q.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "请先输入要配音的文字").into_response();
    }
    let voice = if q.voice.trim().is_empty() {
        "zh-CN-YunxiNeural"
    } else {
        q.voice.trim()
    };
    let rate = if q.rate.trim().is_empty() {
        "+0%"
    } else {
        q.rate.trim()
    };
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    let target = format!(
        "http://127.0.0.1:8095/tts?text={}&voice={}&rate={}",
        enc(&q.text),
        enc(voice),
        enc(rate)
    );

    let resp = match st.client.client.get(&target).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("配音服务不可用: {e}")).into_response(),
    };
    let status = resp.status();
    if !status.is_success() {
        // 上游的错误是 JSON {code,msg}，翻译成人话返回
        let msg = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| format!("配音失败（{status}）"));
        return (StatusCode::BAD_GATEWAY, msg).into_response();
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("读取音频失败: {e}")).into_response(),
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (StatusCode::OK, headers, Body::from(bytes.to_vec())).into_response()
}

/// 可用音色列表（转发 Python 服务）。
async fn voices(State(st): State<AppState>) -> Json<serde_json::Value> {
    match st
        .client
        .client
        .get("http://127.0.0.1:8095/voices")
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({ "code": 500, "msg": format!("{e}") })),
        },
        Err(e) => Json(serde_json::json!({ "code": 500, "msg": format!("配音服务不可用: {e}") })),
    }
}

// ---------- 文案提取（ASR） ----------

#[derive(Debug, Deserialize)]
struct AsrReq {
    url: String,
    #[serde(default)]
    lang: String,
    #[serde(default)]
    max_seconds: u32,
}

/// 语音转文字：转发到本机 ASR 服务（faster-whisper）。
/// 只接受直链——分享页链接请前端先走 /api/quick-parse 拿 video_url。
async fn asr_proxy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AsrReq>,
) -> Json<ApiResp<serde_json::Value>> {
    let user = match ensure_quota(&st, &headers, "asr") {
        Ok(u) => u,
        Err(msg) => return Json(ApiResp::err(msg)),
    };
    let url = pick_url(&q.url);
    let url = url.as_str();
    if url.is_empty() {
        return Json(ApiResp::err("缺少视频直链"));
    }
    if !is_public_url(url) {
        return Json(ApiResp::err("只支持公网视频直链"));
    }

    let enc = |s: &str| {
        percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
    };
    let lang = if q.lang.trim().is_empty() { "zh" } else { q.lang.trim() };
    let mut target = format!("http://127.0.0.1:8097/asr?url={}&lang={}", enc(url), enc(lang));
    if q.max_seconds > 0 {
        // 上限 10 分钟：再长就该切片了，4 核机器扛不住
        target.push_str(&format!("&max_seconds={}", q.max_seconds.min(600)));
    }

    // 转写是慢活（1 分钟音频约 20 秒），默认 12 秒超时必须放大
    let resp = st
        .client
        .client
        .get(&target)
        .timeout(std::time::Duration::from_secs(420))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => return Json(ApiResp::err(format!("转写服务不可用: {e}"))),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Json(ApiResp::err(format!("转写服务响应异常: {e}"))),
    };
    if body.get("code").and_then(|v| v.as_i64()) != Some(200) {
        let msg = body
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("转写失败");
        return Json(ApiResp::err(msg));
    }
    settle(&st, &user, "asr");
    Json(ApiResp::ok(body.get("data").cloned().unwrap_or_default()))
}

// ---------- AI 改写 ----------

#[derive(Debug, Deserialize)]
struct AiReq {
    mode: String,
    text: String,
}

fn ai_prompt(mode: &str, text: &str) -> Option<(String, String)> {
    let system = "你是短视频内容运营，擅长抖音口播与爆款文案。输出中文，直接给结果，不要解释过程、不要客套。";
    let task = match mode {
        "title" => "基于下面这段内容，写 10 个抖音标题：每个不超过 20 字，口语化、有钩子，前缀用 1. 2. 3. 编号。",
        "selling" => "提炼 5 条产品/内容卖点：每条一句话，具体、可感知，避免空话，前缀用 1. 2. 3. 编号。",
        "script" => "改写成一条 30 秒短视频口播脚本（约 120~150 字）：开头 3 秒抓人，中间讲清一个点，结尾引导互动。只输出脚本正文。",
        "rewrite" => "对下面这段文案做伪原创改写：保留信息内核，换结构与用词，避免与原句雷同，读起来像人说的话。只输出改写后的文案。",
        "summary" => "总结下面内容：先给 3~5 条要点（编号），最后给一句话结论。",
        "write" => {
            "根据用户给出的标题、关键词和字数要求，写一篇结构完整、可直接发布的中文文章：\n            有抓人的开头、2~4 个分点正文、有收尾；标题不重复写进正文，不要写大纲，不要自称 AI。\n             只输出正文，段落之间空一行。"
        }
        "digest" => {
            "把下面这段原文压缩成一段通顺的摘要，200 字左右：保留关键信息、数字和结论，\n             不分点、不评价、不添加原文没有的内容，只输出摘要正文。"
        }
        "proofread" => {
            "下面是语音转写出来的文本，可能有同音字、错别字和缺失标点。请修正这些错误并补全标点，\
             保持原句顺序与原意，不要润色、不要改写、不要增删信息；无法确定的地方按最合理理解修正。\
             只输出修正后的文本。"
        }
        _ => return None,
    };
    Some((system.to_string(), format!("{task}\n\n原文：\n{text}")))
}

/// AI 改写：走 OpenAI 兼容接口，Key 从环境变量读。
/// 未配置时返回明确提示，不影响其它功能。
async fn ai_rewrite(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AiReq>,
) -> Json<ApiResp<serde_json::Value>> {
    let user = match ensure_quota(&st, &headers, "ai") {
        Ok(u) => u,
        Err(msg) => return Json(ApiResp::err(msg)),
    };
    let text = req.text.trim();
    if text.is_empty() {
        return Json(ApiResp::err("请先输入原文"));
    }
    if text.chars().count() > 4000 {
        return Json(ApiResp::err("原文过长，请控制在 4000 字以内"));
    }
    let Some((system, user_msg)) = ai_prompt(&req.mode, text) else {
        return Json(ApiResp::err(format!("未知模式: {}", req.mode)));
    };

    let key = std::env::var("AI_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return Json(ApiResp::err(
            "AI 改写还没启用：需要在服务器 .env 里配置 AI_API_KEY（OpenAI 兼容接口，如 DeepSeek）",
        ));
    }
    let base = std::env::var("AI_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com/v1".into());
    let model = std::env::var("AI_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));

    let payload = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user_msg }
        ],
        "temperature": 0.7,
        // 写文章要 2000 字，1200 不够会被截断
        "max_tokens": if req.mode == "write" { 4000 } else { 1200 },
    });

    let resp = st
        .client
        .client
        .post(&url)
        .header(header::AUTHORIZATION, format!("Bearer {}", key.trim()))
        .json(&payload)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => return Json(ApiResp::err(format!("调用大模型失败: {e}"))),
    };
    let status = resp.status();
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Json(ApiResp::err(format!("大模型返回异常: {e}"))),
    };
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Json(ApiResp::err(format!("大模型返回 {status}: {msg}")));
    }
    let content = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() {
        return Json(ApiResp::err("大模型返回为空"));
    }
    settle(&st, &user, "ai");
    Json(ApiResp::ok(serde_json::json!({ "result": content, "mode": req.mode })))
}

// ---------- 管理后台 ----------

/// 后台密码校验：优先用库里存的新密码（argon2 哈希），
/// 没改过则回落到环境变量 ADMIN_PASSWORD（明文，兼容首次部署）。
fn check_admin_password(st: &AppState, input: &str) -> Option<bool> {
    if let Some(hash) = st.db.get_setting("admin_password_hash") {
        return Some(verify_password(input, &hash));
    }
    let env_pw = std::env::var("ADMIN_PASSWORD").unwrap_or_default();
    if env_pw.is_empty() {
        return None; // 完全没配置，禁用后台
    }
    Some(input == env_pw)
}

fn admin_ok(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let tok = raw
        .split(';')
        .map(|p| p.trim())
        .find_map(|p| p.strip_prefix("dy_admin=").map(|s| s.to_string()));
    let Some(tok) = tok else { return false };
    let mut map = st.admin_sessions.lock().unwrap();
    let now = chrono::Local::now().timestamp();
    match map.get(&tok).copied() {
        Some(exp) if exp > now => true,
        Some(_) => {
            map.remove(&tok);
            false
        }
        None => false,
    }
}

fn admin_guard(st: &AppState, headers: &HeaderMap) -> Option<Response> {
    if admin_ok(st, headers) {
        None
    } else {
        Some(json_err(StatusCode::UNAUTHORIZED, "请先登录后台"))
    }
}

#[derive(Debug, Deserialize)]
struct AdminLoginReq {
    password: String,
}

async fn admin_login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AdminLoginReq>,
) -> Response {
    if st
        .db
        .get_setting("admin_password_hash")
        .is_none()
        && std::env::var("ADMIN_PASSWORD").unwrap_or_default().is_empty()
    {
        return json_err(StatusCode::FORBIDDEN, "后台未启用：先在 PM2 环境里设置 ADMIN_PASSWORD");
    }

    // 防爆破：同一 IP 五分钟内最多 10 次
    let ip = client_ip(&headers);
    let now = chrono::Local::now().timestamp();
    {
        let mut fails = st.admin_fails.lock().unwrap();
        let e = fails.entry(ip.clone()).or_insert((now, 0));
        if now - e.0 > 300 {
            *e = (now, 0);
        }
        if e.1 >= 10 {
            return json_err(StatusCode::TOO_MANY_REQUESTS, "尝试太多次，五分钟后再来");
        }
        if check_admin_password(&st, &req.password) != Some(true) {
            e.1 += 1;
            return json_err(StatusCode::UNAUTHORIZED, "密码不对");
        }
    }
    st.admin_fails.lock().unwrap().remove(&ip);

    let token = db::gen_token();
    // 后台会话 8 小时，重启即失效（内存态，够用且不落库）
    st.admin_sessions
        .lock()
        .unwrap()
        .insert(token.clone(), now + 8 * 3600);

    let mut h = HeaderMap::new();
    let cookie = format!(
        "dy_admin={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=28800{}",
        if is_https(&headers) { "; Secure" } else { "" }
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        h.insert(header::SET_COOKIE, v);
    }
    (h, Json(serde_json::json!({ "ok": true }))).into_response()
}

async fn admin_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        if let Some(tok) = raw
            .split(';')
            .map(|p| p.trim())
            .find_map(|p| p.strip_prefix("dy_admin=").map(|s| s.to_string()))
        {
            st.admin_sessions.lock().unwrap().remove(&tok);
        }
    }
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&clear_cookie(is_https(&headers)).replace("dy_sid", "dy_admin"))
    {
        h.insert(header::SET_COOKIE, v);
    }
    (h, Json(serde_json::json!({ "ok": true }))).into_response()
}

#[derive(Debug, Deserialize)]
struct AdminPwdChangeReq {
    old: String,
    new: String,
}

/// 改后台密码：校验旧密码 → 存 argon2 哈希 → 立即生效（当前会话保留）
async fn admin_password_change(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AdminPwdChangeReq>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    match check_admin_password(&st, &req.old) {
        Some(true) => {}
        Some(false) => return json_err(StatusCode::BAD_REQUEST, "当前密码不对"),
        None => return json_err(StatusCode::BAD_REQUEST, "后台未启用密码，请先设置 ADMIN_PASSWORD"),
    }
    if req.new.chars().count() < 8 {
        return json_err(StatusCode::BAD_REQUEST, "新密码至少 8 位");
    }
    if req.new == req.old {
        return json_err(StatusCode::BAD_REQUEST, "新密码和当前密码一样");
    }
    let hash = match hash_password(&req.new) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    match st.db.set_setting("admin_password_hash", &hash) {
        Ok(()) => {
            tracing::info!("admin password changed");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.text()),
    }
}

async fn admin_overview(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    Json(serde_json::json!({ "ok": true, "data": st.db.overview() })).into_response()
}

#[derive(Debug, Deserialize)]
struct AdminUsersQ {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: i64,
    #[serde(default)]
    size: i64,
}

async fn admin_trend(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    Json(serde_json::json!({ "ok": true, "data": st.db.trend(7) })).into_response()
}

const BACKUP_DIR: &str = "backups";

fn list_backups() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(BACKUP_DIR) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".db.gz") {
                continue;
            }
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default();
            out.push(serde_json::json!({
                "name": name,
                "size": meta.len(),
                "time": mtime,
            }));
        }
    }
    out.sort_by(|a, b| b["name"].as_str().unwrap_or("").cmp(a["name"].as_str().unwrap_or("")));
    out
}

async fn admin_backups(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    Json(serde_json::json!({ "ok": true, "data": list_backups() })).into_response()
}

/// 手动触发一次备份（跑 backup.py，和 cron 用的是同一个脚本）
async fn admin_backup_now(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    let result = tokio::task::spawn_blocking(|| {
        std::process::Command::new("python3")
            .arg("backup.py")
            .output()
    })
    .await;

    match result {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if out.status.success() {
                tracing::info!("manual backup ok: {stdout}");
                Json(serde_json::json!({ "ok": true, "data": { "log": stdout } })).into_response()
            } else {
                json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("备份失败：{}", if stderr.is_empty() { stdout } else { stderr }),
                )
            }
        }
        Ok(Err(e)) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("无法执行备份：{e}")),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("备份任务异常：{e}")),
    }
}

async fn admin_users(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AdminUsersQ>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    let size = if q.size > 0 { q.size.clamp(1, 500) } else { 20 };
    let page = q.page.max(1);
    let rows = st.db.list_users(&q.q, size, (page - 1) * size);
    let total = st.db.count_users(&q.q);
    Json(serde_json::json!({
        "ok": true,
        "data": { "rows": rows, "total": total, "page": page, "size": size }
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct AdjustReq {
    email: String,
    delta: i64,
    #[serde(default)]
    note: String,
}

async fn admin_user_adjust(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AdjustReq>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    if req.delta == 0 {
        return json_err(StatusCode::BAD_REQUEST, "加减次数不能为 0");
    }
    let note = if req.note.trim().is_empty() {
        "手动调整".to_string()
    } else {
        req.note.trim().to_string()
    };
    match st.db.adjust_balance(req.email.trim(), req.delta, &note) {
        Ok((email, bal)) => Json(serde_json::json!({
            "ok": true, "data": { "email": email, "balance": bal }
        }))
        .into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.text()),
    }
}

#[derive(Debug, Deserialize)]
struct StatusReq {
    email: String,
    status: String,
}

async fn admin_user_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StatusReq>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    if req.status != "active" && req.status != "banned" {
        return json_err(StatusCode::BAD_REQUEST, "状态只能是 active 或 banned");
    }
    match st.db.set_status(req.email.trim(), &req.status) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.text()),
    }
}

#[derive(Debug, Deserialize)]
struct PwdReq {
    email: String,
    password: String,
}

async fn admin_user_password(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PwdReq>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    if req.password.len() < 6 {
        return json_err(StatusCode::BAD_REQUEST, "密码至少 6 位");
    }
    let hash = match hash_password(&req.password) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    match st.db.set_password(req.email.trim(), &hash) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.text()),
    }
}

#[derive(Debug, Deserialize)]
struct CodesQ {
    #[serde(default)]
    page: i64,
}

async fn admin_codes(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CodesQ>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    let size = 30;
    let page = q.page.max(1);
    let rows = st.db.list_codes(size, (page - 1) * size);
    Json(serde_json::json!({ "ok": true, "data": { "rows": rows, "page": page } })).into_response()
}

#[derive(Debug, Deserialize)]
struct GenCodeReq {
    #[serde(default = "default_quota")]
    quota: i64,
    #[serde(default = "default_count")]
    count: i64,
    #[serde(default)]
    note: String,
    #[serde(default = "default_expire")]
    expire_days: i64,
}

fn default_quota() -> i64 {
    100
}
fn default_count() -> i64 {
    1
}
fn default_expire() -> i64 {
    365
}

async fn admin_code_gen(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<GenCodeReq>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    let count = req.count.clamp(1, 200);
    let quota = req.quota.clamp(1, 100_000);
    let mut made = Vec::new();
    for _ in 0..count {
        match st.db.issue_code(quota, req.note.trim(), "", req.expire_days.clamp(0, 3650)) {
            Ok(code) => made.push(code),
            Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.text()),
        }
    }
    Json(serde_json::json!({ "ok": true, "data": { "codes": made } })).into_response()
}

#[derive(Debug, Deserialize)]
struct DelCodeReq {
    code: String,
}

async fn admin_code_del(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DelCodeReq>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    match st.db.delete_code(req.code.trim()) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.text()),
    }
}

#[derive(Debug, Deserialize)]
struct UsageQ {
    #[serde(default = "default_usage_limit")]
    limit: i64,
    #[serde(default)]
    q: String,
}

fn default_usage_limit() -> i64 {
    100
}

async fn admin_usage(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UsageQ>,
) -> Response {
    if let Some(r) = admin_guard(&st, &headers) {
        return r;
    }
    let rows = st.db.recent_usage(q.limit.clamp(1, 500), &q.q);
    Json(serde_json::json!({ "ok": true, "data": rows })).into_response()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8093);

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/app.db".into());
    let db = Db::open(&db_path).expect("failed to open sqlite");
    tracing::info!("sqlite at {}", db_path);

    let internal_token = std::env::var("INTERNAL_TOKEN").unwrap_or_default();
    if internal_token.is_empty() {
        tracing::warn!("INTERNAL_TOKEN 未设置，内网发码接口已关闭");
    }

    let state = AppState {
        client: Arc::new(DouyinClient::new().expect("failed to build douyin client")),
        db: Arc::new(db),
        internal_token,
        trial: Arc::new(Mutex::new(HashMap::new())),
        admin_sessions: Arc::new(Mutex::new(HashMap::new())),
        admin_fails: Arc::new(Mutex::new(HashMap::new())),
    };

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
        .allow_origin(Any);

    let api = Router::new()
        .route("/health", get(health))
        .route("/api/parse", post(parse))
        .route("/api/fetch", post(fetch))
        .route("/api/cookie/check", post(check_cookie))
        .route("/api/download", get(download))
        .route("/api/img", get(image_proxy))
        .route("/api/zip", post(zip_download))
        .route("/api/tts", get(tts))
        .route("/api/voices", get(voices))
        .route("/api/asr", get(asr_proxy))
        .route("/api/ai", post(ai_rewrite))
        .route("/api/quick-parse", get(quick_parse))
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/me", get(auth_me))
        .route("/api/auth/redeem", post(auth_redeem))
        .route("/api/auth/nickname", post(auth_nickname))
        .route("/internal/issue-code", post(issue_code_internal))
        .route("/api/admin/login", post(admin_login))
        .route("/api/admin/logout", post(admin_logout))
        .route("/api/admin/password", post(admin_password_change))
        .route("/api/admin/overview", get(admin_overview))
        .route("/api/admin/users", get(admin_users))
        .route("/api/admin/user/adjust", post(admin_user_adjust))
        .route("/api/admin/user/status", post(admin_user_status))
        .route("/api/admin/user/password", post(admin_user_password))
        .route("/api/admin/codes", get(admin_codes))
        .route("/api/admin/codes/gen", post(admin_code_gen))
        .route("/api/admin/code/delete", post(admin_code_del))
        .route("/api/admin/usage", get(admin_usage))
        .route("/api/admin/trend", get(admin_trend))
        .route("/api/admin/backups", get(admin_backups))
        .route("/api/admin/backup", post(admin_backup_now))
        .with_state(state)
        .layer(cors);

    let app = Router::new()
        // /admin 直接给后台页面，不用记 admin.html
        .route_service("/admin", ServeFile::new("public/admin.html"))
        .nest_service("/", ServeDir::new("public"))
        .merge(api);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("douyin-web listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
