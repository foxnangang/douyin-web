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
   ├─ /api/quick-parse  →  127.0.0.1:8095 (Python parse-video-py)  免 Cookie，20+ 平台
   ├─ /api/parse        →  Rust 核心 a_bogus 签名                  需 Cookie，单作品
   ├─ /api/fetch        →  Rust 核心 a_bogus 签名                  需 Cookie，主页/点赞批量
   ├─ /api/download     →  流式代理 CDN 视频（带 Referer + Range，解防盗链、可拖动）
   ├─ /api/img          →  缩略图代理（同上，只放行 image/*，缓存 7 天）
   ├─ /api/zip          →  图集打包（后端逐张取回，打不压缩 ZIP，最多 60 张 / 120MB）
   ├─ /api/tts          →  127.0.0.1:8095/tts (edge-tts)           文字转 MP3
   ├─ /api/voices       →  127.0.0.1:8095/voices                   可用音色
   └─ /api/ai           →  OpenAI 兼容接口（AI_API_KEY）           伪原创 / 口播 / 标题 / 卖点
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

## 工具箱功能（2026-11-18 上线）

前端四大工具页，共用一套结果卡片：

| 工具 | 后端 | 说明 |
|---|---|---|
| 去水印 | `/api/quick-parse` | 一个输入框贴任意平台链接，Python 侧 `share_url_source()` 自动识别平台并回传中文名 |
| 抖音批量 | `/api/fetch` | 主页 / 点赞列表，需 Cookie，可限 50/200/500/全部 |
| 文案配音 | `/api/tts` + `/api/voices` | edge-tts，8 个中文音色，语速 ±30%，输出 MP3 |
| AI 改写 | `/api/ai` | 伪原创 / 30秒口播 / 爆款标题 / 提炼卖点 / 要点摘要 / AI 校对 |
| AI 写作 | `/api/ai` (mode=write) | 标题 + 关键词 + 字数档位 → 成文，max_tokens 提到 4000；支持「品牌设定」 |

**品牌设定（2026-11-18）**：每用户一段 1000 字内的背景描述，存 `settings` 表（key = `user_context:<id>`）。
前端在「AI 写作」页折叠区填写，`/api/auth/me` 一起返回，调用 `/api/ai` 时放进 `context` 字段；
后端拼到 system prompt 末尾（`【创作者背景，写作时自然融入，不要生硬复述、不要照抄成广告语】`）。
只作用于 write / digest / 各改写模式，**校对（proofread）不带**，避免它顺手改内容。

实测同一标题：带设定 431 字、开头「总有人问我」，出现「凤竹茶舍」；不带设定 476 字、开头是「朋友圈里有人晒」，品牌零提及。
| 文章摘要 | `/api/ai` (mode=digest) | 长文压成 200 字左右通顺摘要 |

### 分享文案里抠链接（2026-11-18）

抖音「分享」出来是整段文案：`4.64 04/02 q@r.Eh :1pm rEH/ 一只真土鸡…# 土鸡 # 散养鸡 https://v.douyin.com/xxx/`。
直接当 URL 解析会报 `UnsupportedShareUrlError: 分享链接不是有效的 http/https 地址`。

现在三层都加了 `pick_url()`（前端 JS / Rust / Python 各一份，规则一致）：
用正则 `https?://[^\s，,。；;、）)】」”"']+` 抓第一个链接，再去掉尾巴上的中文标点。
纯链接后面紧贴中文（`…/xxx/复制此链接`）也能正确截断。

图集作品会自动出现「打包下载 N 张图」按钮，背景音乐则单独给「下载 BGM」。

### 代理白名单改成「只挡内网」

多平台 CDN 域名太长尾（快手/小红书/B站/微博各家不同），维护精确白名单不现实。
`is_public_url()` 改为：必须 http(s) + 域名带点 + 非裸 IP + 非 localhost/.local/.internal + 端口只能 80/443。
裸 IP 一律拒绝，避免被当成内网探测入口。

取 CDN 资源用两段式：先不带 Referer，失败再补抖音 Referer 重试——
字节系要 Referer，其它家反而对陌生 Referer 敏感。

### AI 改写（已启用 2026-11-18）

后端走 OpenAI 兼容接口，Key 存在 PM2 的应用环境变量里（**不写进仓库**）：

```bash
export AI_API_KEY=sk-...            # 必填
export AI_BASE_URL=https://api.deepseek.com/v1   # 可选，默认就是这个
export AI_MODEL=deepseek-chat       # 可选
pm2 restart dy-web --update-env
pm2 save                            # 必须 save，否则重启/ resurrect 后变量丢失
```

排查方式：`tr '\0' '\n' < /proc/$(pm2 pid dy-web)/environ | grep ^AI_` 能直接看到进程真实环境。

实测（deepseek-chat，同一段明前龙井文案）：

| 模式 | 耗时 | 输出 |
|---|---|---|
| 伪原创改写 | ~1.0s | 41 字，换结构不换信息 |
| 30秒口播 | ~1.1s | 103 字，带钩子和「评论区扣茶字」CTA |
| 爆款标题 | ~1.6s | 10 条，均 ≤20 字 |
| 提炼卖点 | ~1.0s | 5 条，具体可感知 |
| 要点摘要 | ~0.9s | 3 条 + 一句话结论 |

> 火山方舟那套 `ark-…` 的套餐 Key 只能调生图模型，文本模型会报
> `UnsupportedModel: does not support the agent plan feature`，别拿它试 AI 改写。

## 文案提取 / ASR 服务（2026-11-18）

独立进程，跟解析服务分开放——模型常驻占内存，混在一起会互相拖累。

| 项目 | 值 |
|---|---|
| 目录 | `/www/wwwroot/dy.foxnan.com/asr/`（独立 venv） |
| 端口 | `127.0.0.1:8097`（8096 被 qc-workbench 占了，别再踩） |
| PM2 | `dy-asr`，解释器 `./venv/bin/python` |
| 模型 | faster-whisper `small` + int8 + 2 线程，464MB，缓存在 `asr/models/` |
| 下载源 | 必须带 `HF_ENDPOINT=https://hf-mirror.com`，否则连不上 HuggingFace |
| 上限 | 单次 5 分钟音频（`ASR_MAX_SECONDS`，接口可传 `max_seconds` ≤600） |

流水线：`ffmpeg` 直接从 URL 抽音轨（16k 单声道，不落整段视频）→ faster-whisper 转写（带 VAD 切静音）。

> 两个坑：
> 1. ffmpeg 的 `-headers` 要用 CRLF 结尾；防盗链试三轮——带 Referer、只带 UA、什么都不带。
> 2. 4 核 7G 的机器可用内存只有 2~3G，所以 `cpu_threads=2`、单实例串行（`_run_lock`），模型懒加载。

实测：60 秒音频 → 20.5 秒出结果。标准普通话内容准确，但**同音字会错**（茶舍→茶社、一芽一叶→一牙一夜），所以前端配了「AI 校对」按钮走 `/api/ai` 的 `proofread` 模式修一遍。

### 部署 / 重装

```bash
cd /www/wwwroot/dy.foxnan.com/asr
python3 -m venv venv
./venv/bin/pip install fastapi "uvicorn[standard]" faster-whisper
export HF_ENDPOINT=https://hf-mirror.com ASR_MODEL=small ASR_THREADS=2 ASR_PORT=8097
pm2 delete dy-asr; pm2 start asr-server.py --name dy-asr --interpreter ./venv/bin/python
pm2 save
curl -X POST http://127.0.0.1:8097/warmup   # 预下载模型，约 23 秒
```

## 成本模型（2026-11 实测）

按 270 字文案、DeepSeek `deepseek-chat`（= V4-Flash 非思考模式，输入 $0.14 / 输出 $0.28 每百万 token，汇率按 7.1）实测：

| 环节 | 实测 token | 单次成本 |
|---|---|---|
| AI 伪原创改写 | 279 in / 299 out | ≈ ¥0.0009 |
| 30秒口播 | 286 / 127 | ≈ ¥0.0005 |
| 爆款标题 | 284 / 143 | ≈ ¥0.0006 |
| 提炼卖点 | 280 / 193 | ≈ ¥0.0007 |
| 要点摘要 | 264 / 172 | ≈ ¥0.0006 |
| AI 校对 | 307 / 207 | ≈ ¥0.0007 |
| 文案提取（ASR） | 本地 CPU，90 秒音频约 50 CPU·秒 | 边际 ≈ 0 |
| 配音（edge-tts） | — | 免费 |
| 下载原片 | 约 40MB 出网 | 套餐内 1500GB/月 ≈ 3.7 万次 |

一轮「校对 + 改写」≈ **0.16 分**；五个模式全跑一遍 ≈ 0.4 分。每天 20 条，一个月约 ¥1。

真正的瓶颈不是钱，是**并发**：ASR 单实例串行（`_run_lock`），4 核机器同时来 3 个转写请求就得排队。

## 账号 / 邀请码 / 额度（2026-11-18）

SQLite 单文件，不用另起数据库服务。库路径由 `DB_PATH` 决定，默认 `data/app.db`（相对启动目录）。

| 表 | 用途 |
|---|---|
| `users` | 邮箱 + argon2 密码哈希 + 余额 + 状态 |
| `invite_codes` | 码、额度、备注、订单号、有效期、核销人与时间 |
| `sessions` | 登录会话（30 天，HttpOnly Cookie `dy_sid`） |
| `usage_log` | 每次扣费流水，对账用 |

接口：

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/auth/register` | 邀请码 + 邮箱 + 密码，一次性完成注册并登录 |
| POST | `/api/auth/login` | 邮箱 + 密码 |
| POST | `/api/auth/logout` | 清 Cookie |
| GET | `/api/auth/me` | 返回当前邮箱与余额，未登录返回 `ok:false` |
| POST | `/api/auth/redeem` | 老用户充额度（核销新码，余额叠加） |
| POST | `/internal/issue-code` | **内网发码**，需 `X-Internal-Token` 头，同 `order_id` 幂等 |

扣费权重（`quota_cost()`）：解析 1 / 批量 5 / 文案提取 3 / AI 1 / 配音 0 / 图集打包 0 / 下载 0。

未登录：每天 2 次单链接解析（按 IP，内存计数，重启清零），其余接口一律要求登录。

### 环境变量

```bash
export DB_PATH=/www/wwwroot/dy.foxnan.com/data/app.db
export INTERNAL_TOKEN=<32 位随机 hex>     # 不设则内网发码接口关闭
pm2 restart dy-web --update-env && pm2 save
```

### 后台脚本

```bash
cd /www/wwwroot/dy.foxnan.com
python3 admin.py gen-code --quota 100 --count 5 --note 内测
python3 admin.py list-codes          # 看哪些码还没用
python3 admin.py list-users          # 看用户和余额
python3 admin.py grant --email a@b.com --quota 100
python3 admin.py ban --email a@b.com
python3 admin.py stats               # 码用量 + 最近扣费流水
```

### Web 管理后台（2026-11-18）

访问 `https://dy.foxnan.com/admin`，用 `ADMIN_PASSWORD` 登录（不是用户密码）。

```bash
export ADMIN_PASSWORD=<自己起一个强密码>
pm2 restart dy-web --update-env && pm2 save
```

| 页面 | 能做什么 |
|---|---|
| 概览 | 注册用户 / 今日新增 / 近 7 日活跃 / 邀请码已用数与总数 / 今日调用次数 / 未消耗额度合计 + 最近 10 条用量 |
| 用户 | 按邮箱搜、分页；加次数、扣次数、封禁/解封、重置密码 |
| 邀请码 | 批量生成（额度/数量/有效期/备注）、查看状态、删除未使用的码 |
| 用量流水 | 最近 200 条扣费记录（含后台调整） |
| 设置 | 修改管理密码（argon2 哈希存 `settings` 表）+ 数据库备份列表与「立即备份」 |

概览页还有近 7 天调用柱状图；用户页支持昵称列、按邮箱/昵称搜索、导出 CSV；邀请码页可一键复制未用码；流水页可按邮箱/昵称过滤。侧栏底部有「打开前台」。

**管理密码的取值顺序**：库里 `settings.admin_password_hash` → 没有则用环境变量 `ADMIN_PASSWORD`（明文）。
忘记密码时执行 `python3 admin.py reset-admin-password`，会删掉库里的自定义密码，回落到环境变量那个值。

实现要点：

- 后台会话放内存（`HashMap<token, 过期时间>`），8 小时过期，**重启即全部失效**——够用且不落库
- 登录失败同一 IP 5 分钟内最多 10 次，防爆破
- 页面在 `public/admin.html`，`/admin` 由 `ServeFile` 直出；接口全在 `/api/admin/*`，未登录一律 401
- 后台加减次数会写 `usage_log`（kind=`admin:备注`），可追溯

### 博客付款后自动发码（待接）

博客（WordPress + fnic 插件）在同一台机器，付款成功生成 `FN-` 码的那一步追加一次调用即可：

```php
wp_remote_post('http://127.0.0.1:8094/internal/issue-code', [
  'timeout' => 8,
  'headers' => ['Content-Type' => 'application/json', 'X-Internal-Token' => DY_TOKEN],
  'body'    => wp_json_encode(['quota' => 100, 'order_id' => $order_id, 'note' => '博客订单']),
]);
```

返回 `{"ok":true,"data":{"code":"DY-...","reused":false}}`，把 code 一并写进发货邮件即可。
用 `order_id` 做幂等键，重复回调不会重复发码。

## 数据库备份（2026-11-18）

`backup.py`：用 sqlite3 在线备份 API（不是 cp，WAL 模式下也完整）→ `PRAGMA integrity_check` 校验 → gzip → 保留最近 N 份。

```bash
python3 backup.py                    # 手动备一次
python3 backup.py --keep 30          # 保留 30 份
# 定时（已加进 crontab，每天 03:45）
45 3 * * * cd /www/wwwroot/dy.foxnan.com && /usr/bin/python3 backup.py >> logs/backup.log 2>&1
```

- 输出目录 `backups/app-YYYYmmdd-HHMMSS.db.gz`（文件名带秒，同一分钟连备不会互相覆盖）
- 默认保留 14 份，约两周
- 恢复：`gunzip -c backups/xxx.db.gz > data/app.db`，然后 `pm2 restart dy-web`
- 后台「设置」页能看到列表，也能点「立即备份」（走同一个脚本）

## 用户昵称

`users.nickname`，老库启动时自动 `ALTER TABLE` 补列。注册时默认取邮箱前缀；用户点右上角昵称芯片 → 「我的」→ 改昵称（1~16 字）。后台用户列表显示昵称，搜索同时匹配邮箱和昵称。

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
