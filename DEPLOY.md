# 抖音作品解析 · 在线工具（网页版）

## 站点信息

| 项目 | 值 |
|---|---|
| 访问地址 | https://dy.foxnan.com/ |
| HTTPS | ✅ 已启用（ZeroSSL ECC，2026-11-18 签发，有效期至 2027-02-15），80 自动跳转 443 |
| 服务器 | 124.221.136.214 |
| 部署目录 | `/www/wwwroot/dy.foxnan.com/` |
| 后端端口 | 8094（PM2 进程 `douyin-web`） |
| 源码仓库 | https://github.com/foxnangang/douyin-web |

## 双引擎架构（2026-11-18 合并）

```
dy.foxnan.com
   ├─ /api/quick-parse  →  127.0.0.1:8095 (Python parse-video-py)  免 Cookie，分享短链
   ├─ /api/parse        →  Rust 核心 a_bogus 签名                  需 Cookie，单作品
   ├─ /api/fetch        →  Rust 核心 a_bogus 签名                  需 Cookie，主页/点赞批量
   ├─ /api/download     →  流式代理抖音 CDN 视频/图片（带 Referer，解防盗链）
   └─ /api/img          →  缩略图代理（同上，只放行 image/*，缓存 7 天）
```

| 引擎 | 服务 | 用途 | Cookie |
|---|---|---|---|
| 快速解析 | Python venv + PM2 `dy-parser` | 分享短链，33+ 平台，单作品 | 不需要 |
| 深度解析 | Rust 核心 | 主页/点赞/收藏批量，私有内容 | 需要 |

Python 解析服务：
- 路径：`/www/wwwroot/dy.foxnan.com/parser/`（venv 隔离，不污染系统 Python）
- 基线库：`parse-video-py`（MIT，wujunwei928），从 GitHub v0.0.3 tag 安装后强制升级到 main 分支代码
- 两处本地补丁（在 `outbound.py`）：
  - 抖音白名单补 `365yg.com`（西瓜 CDN，抖音偶尔返回）
  - 响应上限 16MB → 64MB（分享页 HTML 超 16MB）
- 监听：`127.0.0.1:8095`（外部无法直连）
- PM2 进程：`dy-parser`
- 上游更新后需重装并重打补丁

下载代理白名单（`CDN_ALLOW`）：`douyinvod.com / douyin.com / douyinpic.com / ixigua.com / 365yg.com / byteimg.com / snssdk.com / bytedance.com / byte-cdn.com / bytecdn.cn / tiktok.com / cdn-ali.tiktok.com`

> 坑：快速解析返回的直链常是 `365yg.com`（西瓜视频 CDN），最初白名单没有它，导致点下载被拦截。2026-11-18 已补。新增平台时需同步检查白名单。

## 缩略图代理（2026-11-18）

列表卡片封面统一走 `/api/img?url=<encodeURIComponent(封面地址)>`：

- 抖音图片 CDN（`douyinpic.com` 等）校验 `Referer`，浏览器 `<img>` 直连常 403，且跨域不稳定
- 后端带 `Referer: https://www.douyin.com/` + 真实 UA 取回字节，`Cache-Control: public, max-age=604800, immutable`，反复翻列表不回源
- 只放行 `image/*` 类型 + 同一个 `CDN_ALLOW` 白名单，防止被当成开放代理
- 封面字段来源：`TaskRow.cover`（视频取 `video.origin_cover` 优先，图集取该图 `url_list[0]` 低清版）；快速解析取 Python 返回的 `cover_url`
- 前端 `img` 加载失败自动隐藏，露出「暂无预览」占位，不弹错

## 下载代理的 Range 转发

`/api/download` 会透传客户端的 `Range` 头，并在 206 响应里回带 `Content-Range`：

- 不转发时浏览器只能整段下载，`<video>` 进度条拖不动
- 转发后「打开」页可边下边播、任意跳转
- 实测同一条 365yg 直链：完整下载 42MB / `video/mp4`，字节头 `ftypisom` 正常

## 功能

1. **单链接解析**：粘贴作品链接 → 返回无水印直链（视频/图集/实况），可在线打开或下载
2. **主页/点赞批量解析**：粘贴用户主页，批量列出作品，支持条数限制（50/200/500/全部）
3. **Cookie 校验**：粘贴抖音 Cookie，校验有效性并显示账号昵称

## 架构

```
浏览器 → Nginx(80) → 127.0.0.1:8094 → Rust HTTP 服务(axum)
                                          ├─ a_bogus 签名（SM3+RC4+自定义Base64）
                                          ├─ 抖音 aweme/v1/web/ 接口
                                          └─ 静态页面 public/
```

- 后端复用 `douyin-downloader` v4.0 的签名与接口层（`douyin/` 模块原样复制），未重写算法
- 服务器**无需安装 Rust**：二进制由 GitHub Actions 在云端编译（ubuntu-latest + dtolnay/rust-toolchain）
- 前端为单文件 HTML，墨绿主题，与桌面定制版视觉一致

## 重要限制：必须提供 Cookie

抖音接口有 `ArgusSecurityPlugin` 风控，服务端无 Cookie 请求会被拦：

```
HTTP 403 Blocked by ArgusSecurityPlugin Uifid Not Found
```

这与原桌面版一致（桌面版也要扫码登录后才工作）。网页版因浏览器无法读取 HttpOnly Cookie，**改为用户手动粘贴 Cookie**。

获取方式：浏览器登录抖音 → F12 → Network → 任意请求 → Request Headers → 复制整段 Cookie。

## 部署与运维

### 重新部署

```bash
# 本地：改代码 → 推送
git push origin main    # 触发 Actions 编译

# 下载 artifact 后上传到服务器
scp douyin-web-server root@124.221.136.214:/www/wwwroot/dy.foxnan.com/
scp public/index.html root@124.221.136.214:/www/wwwroot/dy.foxnan.com/public/

# 服务器：重启
pm2 restart douyin-web
```

### 端口占用说明

- 8093 已被 `lead_gen_api` 占用，故本服务用 **8094**
- 二进制默认端口 8093，由 PM2 启动时用 `PORT=8094` 环境变量覆盖

### HTTPS（2026-11-18 已完成）

- 签发：ZeroSSL ECC DV SSL，有效期 2026-11-17 至 2027-02-15
- 自动续期：acme.sh 已配置，下次续期窗口由 ARI 指定（2027-02-01 前后）
- 证书路径：`/www/server/panel/vhost/cert/dy.foxnan.com/{fullchain.pem,privkey.pem}`
- Nginx：443 server 块已启用，80 端口 301 跳转 HTTPS
- 配置备份：`/www/server/panel/vhost/nginx/dy.foxnan.com.conf.bak-20261118`

历史说明：首次用 `--force` 申请触发 CA 限流（`retryafter=86400`），约 1 小时后不带 `--force` 重试即成功。教训：域名首次签发不要用 `--force`。

## 验证记录（2026-11-18）

| 项目 | 结果 |
|---|---|
| GitHub Actions 编译 | ✅ success |
| 二进制本地启动 | ✅ 健康检查通过 |
| PM2 进程 | ✅ online，0 重启 |
| 首页访问 | ✅ HTTPS HTTP 200，14542 bytes |
| 公网访问 | ✅ HTTPS 200，证书链验证通过（ssl_verify_result=0） |
| HTTP 跳转 | ✅ 80 端口 301 → https://dy.foxnan.com/ |
| HTTPS | ✅ ZeroSSL ECC，有效期至 2027-02-15 |
| 单链接解析（无 Cookie） | ❌ 抖音风控 403（预期行为，需 Cookie） |
| API 错误处理 | ✅ 返回明确错误 |
| HTTPS | ⏳ CA 限流待签 |

## 法律声明

- 本工具仅提取公开直链，请勿用于侵权用途
- 原项目 LICENSE（MIT + 禁止商用附加条款）保留在桌面版仓库中
- 网页版复用其签名算法（源自 f2，Apache-2.0）
