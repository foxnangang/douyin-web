#!/usr/bin/env python3
"""SQLite 每日备份：一致性快照 → gzip → 保留最近 N 份。

用 sqlite3 的在线备份 API（不是 cp），WAL 模式下拷出来也是完整的；
备份完顺手跑一次 integrity_check，坏了当场报错而不是等到要恢复时才发现。

用法：
    python3 backup.py                # 正常备份
    python3 backup.py --keep 30      # 保留 30 份
    python3 backup.py --dir /path    # 指定输出目录

cron（每天 03:30）：
    30 3 * * * cd /www/wwwroot/dy.foxnan.com && /usr/bin/python3 backup.py >> logs/backup.log 2>&1
"""
import argparse
import gzip
import os
import shutil
import sqlite3
import sys
from datetime import datetime

BASE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_DB = os.environ.get("DB_PATH", os.path.join(BASE, "data", "app.db"))


def backup(db_path: str, out_dir: str, keep: int) -> str:
    if not os.path.exists(db_path):
        sys.exit(f"数据库不存在：{db_path}")
    os.makedirs(out_dir, exist_ok=True)

    stamp = datetime.now().strftime("%Y%m%d-%H%M")
    tmp = os.path.join(out_dir, f".app-{stamp}.db")
    final = os.path.join(out_dir, f"app-{stamp}.db.gz")

    # 1) 在线备份成一份普通 db 文件
    src = sqlite3.connect(db_path, timeout=20)
    dst = sqlite3.connect(tmp)
    try:
        # pages=-1 一次拷完；WAL 里未 checkpoint 的内容也会带上
        src.backup(dst)
    finally:
        dst.close()
        src.close()

    # 2) 完整性校验，坏了直接报错（别把坏备份当成功）
    chk = sqlite3.connect(tmp)
    try:
        row = chk.execute("PRAGMA integrity_check").fetchone()
        result = row[0] if row else "unknown"
    finally:
        chk.close()
    if result != "ok":
        os.remove(tmp)
        sys.exit(f"备份完整性校验失败：{result}")

    # 3) 压缩
    with open(tmp, "rb") as f_in, gzip.open(final, "wb", compresslevel=6) as f_out:
        shutil.copyfileobj(f_in, f_out)
    os.remove(tmp)

    size = os.path.getsize(final)
    # 4) 清理老备份
    olds = sorted(f for f in os.listdir(out_dir) if f.startswith("app-") and f.endswith(".db.gz"))
    removed = 0
    for f in olds[:-keep] if keep > 0 else []:
        os.remove(os.path.join(out_dir, f))
        removed += 1

    print(f"{datetime.now():%Y-%m-%d %H:%M:%S} 备份完成 {os.path.basename(final)} "
          f"{size / 1024:.0f}KB 校验ok 清理{removed}份 现存{min(len(olds), keep) if keep > 0 else len(olds)}份")
    return final


def main():
    ap = argparse.ArgumentParser(description="dy.foxnan.com 数据库备份")
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--dir", default=os.path.join(BASE, "backups"))
    ap.add_argument("--keep", type=int, default=14, help="保留最近几份，0 表示不清理")
    args = ap.parse_args()
    backup(args.db, args.dir, args.keep)


if __name__ == "__main__":
    main()
