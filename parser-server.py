"""轻量解析 + 配音服务：包装 parse-video-py 与 edge-tts，只监听本机，供 dy.foxnan.com 内部调用。"""
import asyncio
import json
import re
from dataclasses import asdict, is_dataclass

import edge_tts
import uvicorn
from fastapi import FastAPI
from fastapi.responses import JSONResponse, Response

from parse_video_py import parse_video_share_url
from parse_video_py.parser import share_url_source

app = FastAPI(title='Parser', version='2.0.0')

# 平台中文名。key 与 parse_video_py.parser.VideoSource.value 一致
PLATFORM_LABEL = {
    'douyin': '抖音',
    'kuaishou': '快手',
    'pipixia': '皮皮虾',
    'weibo': '微博',
    'weishi': '微视',
    'lvzhou': '绿洲',
    'zuiyou': '最右',
    'quanmin': '度小视',
    'xigua': '西瓜视频',
    'lishipin': '梨视频',
    'pipigaoxiao': '皮皮搞笑',
    'huya': '虎牙',
    'acfun': 'AcFun',
    'doupai': '逗拍',
    'meipai': '美拍',
    'quanminkge': '全民K歌',
    'sixroom': '六间房',
    'xinpianchang': '新片场',
    'haokan': '好看视频',
    'bilibili': '哔哩哔哩',
    'redbook': '小红书',
    'twitter': 'Twitter/X',
    'qqvideo': '腾讯视频',
    'sohu': '搜狐视频',
    'cctv': '央视网',
}

# 只暴露质量稳定的中文音色；顺序 = 前端下拉顺序
VOICES = [
    {'id': 'zh-CN-YunxiNeural', 'name': '云希 · 男声 年轻自然', 'desc': '推荐，短视频口播'},
    {'id': 'zh-CN-XiaoxiaoNeural', 'name': '晓晓 · 女声 温柔', 'desc': '亲和，生活类'},
    {'id': 'zh-CN-XiaoyiNeural', 'name': '晓伊 · 女声 活泼', 'desc': '轻快，种草类'},
    {'id': 'zh-CN-YunyangNeural', 'name': '云扬 · 男声 播音', 'desc': '专业，资讯类'},
    {'id': 'zh-CN-YunjianNeural', 'name': '云健 · 男声 浑厚', 'desc': '有力，带货类'},
    {'id': 'zh-CN-YunxiaNeural', 'name': '云夏 · 男童', 'desc': '童声'},
    {'id': 'zh-CN-liaoning-XiaobeiNeural', 'name': '晓北 · 东北话', 'desc': '方言，搞笑'},
    {'id': 'zh-CN-shaanxi-XiaoniNeural', 'name': '晓妮 · 陕西话', 'desc': '方言'},
]

RATE_RE = re.compile(r'^[+-]\d{1,3}%$')


def dump(obj):
    if is_dataclass(obj):
        return asdict(obj)
    if hasattr(obj, 'model_dump'):
        return obj.model_dump()
    if isinstance(obj, dict):
        return obj
    return json.loads(json.dumps(obj, default=str))


@app.get('/health')
async def health():
    return {'ok': True, 'service': 'parser', 'version': '2.0.0'}


@app.get('/parse')
async def parse(url: str):
    if not url or len(url) > 500:
        return JSONResponse({'code': 400, 'msg': '链接为空或过长'}, status_code=400)

    # 平台识别放在解析之前：识别不出来就直接给友好提示，不用等超时
    platform = ''
    try:
        platform = share_url_source(url).value
    except Exception:
        platform = ''

    try:
        result = await asyncio.wait_for(parse_video_share_url(url), timeout=30)
        data = dump(result)
        data['platform'] = platform
        data['platform_label'] = PLATFORM_LABEL.get(platform, platform or '未知平台')
        return {'code': 200, 'msg': 'ok', 'data': data}
    except asyncio.TimeoutError:
        return JSONResponse({'code': 408, 'msg': '解析超时，请稍后重试'}, status_code=408)
    except Exception as e:
        msg = str(e)
        if '不属于支持的平台' in msg:
            friendly = '暂不支持这个平台的链接，可试试抖音 / 快手 / 小红书 / B站 等'
        elif 'Failed to parse video ID' in msg or 'parse video ID' in msg:
            friendly = '链接已失效或不完整：请重新从 App 分享，复制完整短链（形如 v.douyin.com/xxxxx/）'
        elif 'Too Large' in type(e).__name__ or '超出' in msg:
            friendly = '分享页过大，解析超时，请稍后重试'
        elif '412' in msg:
            friendly = '目标平台风控拦截（412），这个平台暂时解析不了，可换个链接或稍后再试'
        else:
            friendly = f'{type(e).__name__}: {msg[:150]}'
        return JSONResponse({'code': 500, 'msg': friendly}, status_code=500)


@app.get('/voices')
async def voices():
    return {'code': 200, 'data': VOICES}


@app.get('/tts')
async def tts(text: str, voice: str = 'zh-CN-YunxiNeural', rate: str = '+0%'):
    text = (text or '').strip()
    if not text:
        return JSONResponse({'code': 400, 'msg': '文本为空'}, status_code=400)
    if len(text) > 2000:
        return JSONResponse({'code': 400, 'msg': '文本过长，单次最多 2000 字'}, status_code=400)
    if voice not in {v['id'] for v in VOICES}:
        return JSONResponse({'code': 400, 'msg': '不支持的音色'}, status_code=400)
    if not RATE_RE.match(rate):
        rate = '+0%'

    try:
        communicate = edge_tts.Communicate(text, voice, rate=rate)
        buf = bytearray()
        async for chunk in communicate.stream():
            if chunk['type'] == 'audio':
                buf.extend(chunk['data'])
        if not buf:
            return JSONResponse({'code': 500, 'msg': '合成失败：没有返回音频'}, status_code=500)
        return Response(
            content=bytes(buf),
            media_type='audio/mpeg',
            headers={'Cache-Control': 'no-store'},
        )
    except Exception as e:
        return JSONResponse(
            {'code': 500, 'msg': f'合成失败：{type(e).__name__}: {str(e)[:120]}'},
            status_code=500,
        )


if __name__ == '__main__':
    uvicorn.run(app, host='127.0.0.1', port=8095, log_level='warning')
