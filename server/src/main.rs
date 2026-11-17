//! 抖音解析网页版后端。
//! 复用 douyin-downloader 的 a_bogus 签名与接口层，对外提供 HTTP JSON API。
//! 前端静态资源由 public/ 目录提供，Nginx 可反代本服务。

mod douyin;
mod error;
mod models;
mod paths;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, Method};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
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
