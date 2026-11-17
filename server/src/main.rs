//! 抖音解析网页版后端。
//! 复用 douyin-downloader 的 a_bogus 签名与接口层，对外提供 HTTP JSON API。
//! 前端静态资源由 public/ 目录提供，Nginx 可反代本服务。

mod douyin;
mod error;
mod models;
mod paths;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{body::Body, Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

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
}

fn mode_from(s: &str) -> Result<FetchMode, AppError> {
    FetchMode::from_slug(s)
        .ok_or_else(|| AppError::msg(format!("未知模式: {s}，可用 single/post/favorite/collect")))
}

// ---------- 处理函数 ----------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true, "service": "douyin-web", "version": "1.0.0" }))
}

/// 单链接解析：返回作品直链与元数据（无水印源地址）。
async fn parse(
    State(st): State<AppState>,
    Json(req): Json<ParseReq>,
) -> Json<ApiResp<ParseData>> {
    let url = req.url.trim();
    if url.is_empty() {
        return Json(ApiResp::err("请粘贴作品链接"));
    }
    match st.client.fetch_plain(url, &req.cookie, FetchMode::Single).await {
        Ok((profile, list)) => {
            let works = to_rows(&profile, &list);
            let count = works.len();
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
    Json(req): Json<FetchReq>,
) -> Json<ApiResp<ParseData>> {
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
    match st.client.fetch_plain(&req.url, &req.cookie, mode).await {
        Ok((profile, mut list)) => {
            if req.limit > 0 && list.len() > req.limit {
                list.truncate(req.limit);
            }
            let works = to_rows(&profile, &list);
            let count = works.len();
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

/// 流式代理下载抖音 CDN 文件。
/// 浏览器直连 CDN 会因 Referer 校验返回 403；由后端带正确 Referer 转发即可。
async fn download(
    State(st): State<AppState>,
    Query(q): Query<DownloadReq>,
) -> Response {
    if !is_allowed_cdn(&q.url) {
        return (
            StatusCode::BAD_REQUEST,
            "仅允许代理抖音 CDN 域名".to_string(),
        )
            .into_response();
    }

    let resp = st
        .client
        .client
        .get(&q.url)
        .header(header::REFERER, "https://www.douyin.com/")
        .header(header::USER_AGENT, douyin::api::UA)
        .send()
        .await;

    let upstream = match resp {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("请求 CDN 失败: {e}")).into_response()
        }
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
struct QuickReq {
    url: String,
}

/// 免 Cookie 快速解析：本机 Python 解析服务（parse-video-py）转发。
/// 只接受抖音/主流平台分享短链，结果映射成前端统一的作品结构。
async fn quick_parse(
    State(st): State<AppState>,
    Query(q): Query<QuickReq>,
) -> Json<ApiResp<serde_json::Value>> {
    if q.url.trim().is_empty() {
        return Json(ApiResp::err("请粘贴分享链接"));
    }
    let target = format!(
        "http://127.0.0.1:8095/parse?url={}",
        percent_encoding::utf8_percent_encode(&q.url, percent_encoding::CONTROLS)
    );
    let resp = st.client.client.get(&target).send().await;
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
    // 映射成前端统一的作品行（TaskRow 风格），无水印直链放 url 字段
    let row = serde_json::json!({
        "id": "quick-1",
        "awemeId": "quick",
        "desc": d.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "kind": "video",
        "url": d.get("video_url").and_then(|v| v.as_str()).unwrap_or(""),
        "playUrl": d.get("video_url").and_then(|v| v.as_str()).unwrap_or(""),
        "ext": "mp4",
        "isImage": false,
        "date": "",
        "mix": "",
        "ownerNickname": d.get("author").and_then(|a| a.get("name")).and_then(|v| v.as_str()).unwrap_or(""),
        "urlHash": "quick",
    });
    let profile = serde_json::json!({
        "nickname": d.get("author").and_then(|a| a.get("name")).and_then(|v| v.as_str()).unwrap_or(""),
        "uniqueId": "",
        "avatarUrl": d.get("author").and_then(|a| a.get("avatar")).and_then(|v| v.as_str()).unwrap_or(""),
        "secUid": "",
    });
    Json(ApiResp::ok(serde_json::json!({
        "profile": profile,
        "works": [row],
        "count": 1,
        "mode": "quick",
        "coverUrl": d.get("cover_url").and_then(|v| v.as_str()).unwrap_or(""),
    })))
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

    let state = AppState {
        client: Arc::new(DouyinClient::new().expect("failed to build douyin client")),
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
        .route("/api/quick-parse", get(quick_parse))
        .with_state(state)
        .layer(cors);

    let app = Router::new()
        .nest_service("/", ServeDir::new("public"))
        .merge(api);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("douyin-web listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
