# 抖音作品解析 · 网页版

在线工具：http://dy.foxnan.com/

## 这个项目是什么

把桌面版 `douyin-downloader` 的**解析核心**包成 HTTP 服务，再配一个网页界面。用户在浏览器粘贴链接即可提取无水印直链，无需安装软件。

## 代码来源（重要）

本项目**衍生自** [yanruying/douyin-downloader](https://github.com/yanruying/douyin-downloader)，原作者 颜如嘤 (YanRuYing)。

| 部分 | 来源 | 说明 |
|---|---|---|
| `server/src/douyin/` 全套（6 文件） | 原项目 | 签名、抖音接口、解析，**逐字节复制，零修改** |
| `server/src/models.rs`、`paths.rs` | 原项目 | 数据模型与工具函数，零修改 |
| `server/src/main.rs` | 本项目新写 | axum HTTP 服务、API 端点、下载代理 |
| `server/src/error.rs` | 原项目改造 | 去掉 Tauri 错误变体 |
| `public/index.html` | 本项目新写 | 网页界面（墨绿主题） |
| `.github/workflows/build.yml` | 本项目新写 | 云端编译配置 |

原项目核心代码占后端约 **88%**（2681 行 / 3054 行）。签名算法 a_bogus 源自 [Johnserf-Seed/f2](https://github.com/Johnserf-Seed/f2)（Apache-2.0），由原作者引入。

**许可证**：见 [LICENSE](LICENSE)。原项目的 MIT 授权与附加使用限制条款完整保留。本衍生项目同样仅供学习与研究，禁止商业用途。

## 功能

1. 单链接解析：作品页地址或短链 → 无水印直链（视频/图集/实况）
2. 主页/点赞批量解析：按条数限制列出作品
3. Cookie 校验：粘贴抖音 Cookie 后显示账号信息
4. 后端代理下载：规避抖音 CDN 防盗链，支持在线预览与下载

## 技术架构

```
浏览器 index.html
   ↓ POST /api/parse | /api/fetch | /api/cookie/check
axum HTTP 服务 (main.rs)
   ↓ 调用
douyin 核心（原作者代码）：a_bogus 签名 → aweme/v1/web 接口 → JSON 解析
   ↓
抖音服务器

下载单独走 /api/download（流式代理，带抖音 Referer，CDN 域名白名单）
```

- Rust 核心由 GitHub Actions 在云端编译为 Linux 二进制，服务器无需安装 Rust
- 原桌面版的 a_bogus 算法有对拍测试，重写风险极高，故选择直接复用而非移植到 Node/Python

## 使用限制

抖音接口有 `ArgusSecurityPlugin` 风控，服务端无 Cookie 请求会被拦（403）。**使用前必须粘贴有效抖音 Cookie**。这与桌面版"先扫码登录"是同一限制。

## 部署

见 [DEPLOY.md](DEPLOY.md)。服务器 124.221.136.214，端口 8094，PM2 进程 `douyin-web`。

## 验证记录

- GitHub Actions 编译：通过
- 首页 / health：HTTP 200
- 下载代理白名单：非 CDN 域名 400，CDN 域名正常透传
- HTTPS：待 CA 限流解除后签发
