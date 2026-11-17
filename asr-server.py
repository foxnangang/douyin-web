"""文案提取（语音转文字）服务：faster-whisper + ffmpeg，只监听本机。

独立于解析服务：模型常驻约 500MB 内存，和轻量的解析服务混在一起会互相拖累。
"""
import asyncio
import os
import shutil
import subprocess
import tempfile

import uvicorn
from fastapi import FastAPI
from fastapi.responses import JSONResponse

MODEL_SIZE = os.environ.get('ASR_MODEL', 'small')
CPU_THREADS = int(os.environ.get('ASR_THREADS', '2'))
MAX_SECONDS = int(os.environ.get('ASR_MAX_SECONDS', '300'))
UA = (
    'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 '
    '(KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1'
)

app = FastAPI(title='ASR', version='1.0.0')

_model = None
_model_lock = asyncio.Lock()
# 同一时刻只跑一个转写：4 核机器上并发跑只会互相抢 CPU
_run_lock = asyncio.Lock()


def load_model():
    """懒加载：没有请求进来就不占内存。"""
    global _model
    if _model is None:
        from faster_whisper import WhisperModel

        _model = WhisperModel(
            MODEL_SIZE,
            device='cpu',
            compute_type='int8',
            cpu_threads=CPU_THREADS,
            download_root=os.path.join(os.path.dirname(__file__), 'models'),
        )
    return _model


def extract_audio(url: str, out_path: str, max_seconds: int) -> None:
    """用 ffmpeg 直接从 URL 抽音轨（16k 单声道），不落整段视频。

    CDN 有防盗链，先带 Referer 试，失败再不带头重试——各家策略不一样。
    """
    out_opts = ['-t', str(max_seconds), '-vn', '-ac', '1', '-ar', '16000', '-f', 'wav', out_path]
    attempts = [
        ['-headers', 'Referer: https://www.douyin.com/\r\n', '-user_agent', UA],
        ['-user_agent', UA],
        [],
    ]
    last_err = ''
    for extra in attempts:
        cmd = ['ffmpeg', '-y', '-loglevel', 'error'] + extra + ['-i', url] + out_opts
        r = subprocess.run(cmd, capture_output=True, timeout=max_seconds + 120)
        if r.returncode == 0 and os.path.exists(out_path) and os.path.getsize(out_path) > 2000:
            return
        last_err = (r.stderr or b'').decode('utf-8', 'replace')[:300]
    raise RuntimeError(f'音频提取失败：{last_err or "未知原因"}')


def transcribe_sync(wav_path: str, lang: str):
    model = load_model()
    # 中文场景喂一句引导：Whisper 不给引导时常输出繁体、且几乎不打标点
    prompt = '以下是普通话的句子，使用简体中文，并加上标点。' if lang in ('zh', '') else None
    segments, info = model.transcribe(
        wav_path,
        language=None if lang in ('', 'auto') else lang,
        beam_size=1,
        vad_filter=True,
        initial_prompt=prompt,
        vad_parameters={'min_silence_duration_ms': 400},
    )
    parts = []
    timing = []
    for s in segments:
        text = (s.text or '').strip()
        if not text:
            continue
        parts.append(text)
        timing.append({'start': round(s.start, 2), 'end': round(s.end, 2), 'text': text})
    return ' '.join(parts).strip(), timing, round(getattr(info, 'duration', 0) or 0, 1)


@app.get('/health')
async def health():
    return {'ok': True, 'service': 'asr', 'model': MODEL_SIZE, 'loaded': _model is not None}


@app.get('/asr')
async def asr(url: str, lang: str = 'zh', max_seconds: int = 0):
    if not url or len(url) > 2000:
        return JSONResponse({'code': 400, 'msg': '链接为空或过长'}, status_code=400)
    limit = MAX_SECONDS
    if max_seconds and 0 < max_seconds < limit:
        limit = max_seconds

    tmpdir = tempfile.mkdtemp(prefix='asr-')
    wav = os.path.join(tmpdir, 'a.wav')
    try:
        async with _run_lock:
            try:
                await asyncio.to_thread(extract_audio, url, wav, limit)
            except subprocess.TimeoutExpired:
                return JSONResponse({'code': 408, 'msg': '音频提取超时，视频可能太长'}, status_code=408)
            except Exception as e:
                return JSONResponse({'code': 500, 'msg': str(e)[:200]}, status_code=500)

            try:
                text, timing, duration = await asyncio.to_thread(transcribe_sync, wav, lang)
            except Exception as e:
                return JSONResponse(
                    {'code': 500, 'msg': f'转写失败：{type(e).__name__}: {str(e)[:150]}'},
                    status_code=500,
                )

        if not text:
            return JSONResponse({'code': 200, 'msg': 'ok', 'data': {
                'text': '', 'segments': [], 'duration': duration, 'model': MODEL_SIZE}},
                status_code=200)
        return {'code': 200, 'msg': 'ok', 'data': {
            'text': text, 'segments': timing, 'duration': duration, 'model': MODEL_SIZE}}
    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)


@app.post('/warmup')
async def warmup():
    """预加载模型，避免第一次请求等太久（部署后手动调一次）。"""
    async with _model_lock:
        await asyncio.to_thread(load_model)
    return {'ok': True, 'model': MODEL_SIZE}


if __name__ == '__main__':
    uvicorn.run(app, host='127.0.0.1', port=int(os.environ.get('ASR_PORT', '8096')), log_level='warning')
