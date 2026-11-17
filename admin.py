#!/usr/bin/env python3
"""dy.foxnan.com 后台脚本：发码、看用户、加额度、封号。

直接读写 SQLite（WAL 模式，和应用并发安全），不用起服务。

用法：
    python3 admin.py gen-code --quota 100 --count 5 --note 内测
    python3 admin.py list-codes
    python3 admin.py list-users
    python3 admin.py grant --email a@b.com --quota 100
    python3 admin.py ban --email a@b.com
    python3 admin.py unban --email a@b.com
    python3 admin.py stats
"""
import argparse
import os
import secrets
import sqlite3
import sys
from datetime import datetime, timedelta

DB = os.environ.get("DB_PATH", os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "app.db"))

# 去掉容易看错的 0/O/1/I/L
ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"


def conn():
    if not os.path.exists(DB):
        sys.exit(f"数据库不存在：{DB}（先启动一次服务让它建表）")
    c = sqlite3.connect(DB, timeout=10)
    c.execute("PRAGMA busy_timeout=5000")
    return c


def new_code() -> str:
    body = "".join(secrets.choice(ALPHABET) for _ in range(12))
    return f"DY-{body[0:4]}-{body[4:8]}-{body[8:12]}"


def cmd_gen_code(args):
    c = conn()
    expire = None
    if args.expire_days > 0:
        expire = (datetime.now() + timedelta(days=args.expire_days)).strftime("%Y-%m-%d %H:%M:%S")
    made = []
    for _ in range(args.count):
        for _try in range(10):
            code = new_code()
            try:
                c.execute(
                    "INSERT INTO invite_codes (code, quota, note, order_id, expire_at, created_at)"
                    " VALUES (?,?,?,?,?,?)",
                    (code, args.quota, args.note, args.order_id, expire,
                     datetime.now().strftime("%Y-%m-%d %H:%M:%S")),
                )
                made.append(code)
                break
            except sqlite3.IntegrityError:
                continue
    c.commit()
    print(f"生成 {len(made)} 个码，每个 {args.quota} 次：")
    for m in made:
        print("  ", m)


def cmd_list_codes(args):
    c = conn()
    rows = c.execute(
        "SELECT code, quota, CASE WHEN redeemed_by IS NULL THEN '' ELSE 'used' END, expire_at, note"
        "  FROM invite_codes ORDER BY created_at DESC LIMIT ?",
        (args.limit,),
    ).fetchall()
    print(f"{'邀请码':22s} {'额度':>6s} {'状态':>6s} {'有效期':12s} 备注")
    for code, quota, used, expire, note in rows:
        print(f"{code:22s} {quota:>6d} {('已用' if used else '未用'):>6s} {(expire or '长期')[:10]:12s} {note}")


def cmd_list_users(args):
    c = conn()
    rows = c.execute(
        "SELECT id, email, balance, status, created_at, last_login_at FROM users ORDER BY id DESC LIMIT ?",
        (args.limit,),
    ).fetchall()
    print(f"{'ID':>4s} {'邮箱':32s} {'余额':>6s} {'状态':6s} {'注册时间':19s} 最近登录")
    for uid, email, bal, status, created, last in rows:
        print(f"{uid:>4d} {email:32s} {bal:>6d} {status:6s} {created:19s} {last or '-'}")


def find_user(c, email):
    row = c.execute("SELECT id, email FROM users WHERE email = ?", (email.lower().strip(),)).fetchone()
    if not row:
        sys.exit(f"没有这个用户：{email}")
    return row


def cmd_grant(args):
    c = conn()
    uid, email = find_user(c, args.email)
    c.execute("UPDATE users SET balance = balance + ? WHERE id = ?", (args.quota, uid))
    c.execute(
        "INSERT INTO usage_log (user_id, kind, cost, balance_after, created_at)"
        " VALUES (?, 'admin-grant', ?, (SELECT balance FROM users WHERE id = ?), ?)",
        (uid, -args.quota, uid, datetime.now().strftime("%Y-%m-%d %H:%M:%S")),
    )
    c.commit()
    bal = c.execute("SELECT balance FROM users WHERE id = ?", (uid,)).fetchone()[0]
    print(f"{email} 加 {args.quota} 次，当前余额 {bal}")


def cmd_ban(args, status="banned"):
    c = conn()
    uid, email = find_user(c, args.email)
    c.execute("UPDATE users SET status = ? WHERE id = ?", (status, uid))
    c.commit()
    print(f"{email} -> {status}")


def cmd_stats(args):
    c = conn()
    total_codes = c.execute("SELECT COUNT(*) FROM invite_codes").fetchone()[0]
    used_codes = c.execute("SELECT COUNT(*) FROM invite_codes WHERE redeemed_by IS NOT NULL").fetchone()[0]
    users = c.execute("SELECT COUNT(*) FROM users").fetchone()[0]
    bal = c.execute("SELECT COALESCE(SUM(balance),0) FROM users").fetchone()[0]
    print(f"邀请码：{total_codes} 个，已用 {used_codes} 个")
    print(f"用户：{users} 个，未消耗额度合计 {bal} 次")
    print("\n最近 10 条用量：")
    rows = c.execute(
        "SELECT u.email, l.kind, l.cost, l.balance_after, l.created_at"
        "  FROM usage_log l LEFT JOIN users u ON u.id = l.user_id"
        " ORDER BY l.id DESC LIMIT 10"
    ).fetchall()
    for email, kind, cost, after, ts in rows:
        print(f"  {ts}  {email or '-':30s} {kind:12s} -{cost:<4d} 余 {after}")


def cmd_reset_admin(args):
    """忘记管理密码时用：清掉库里自定义的密码，回落到环境变量 ADMIN_PASSWORD。"""
    c = conn()
    n = c.execute("DELETE FROM settings WHERE k = 'admin_password_hash'").rowcount
    c.commit()
    if n:
        print("已清除自定义管理密码 → 后台密码回到 ADMIN_PASSWORD 环境变量的值")
        print("（该值同时存在 /www/wwwroot/dy.foxnan.com/.admin_password）")
    else:
        print("库里没有自定义密码，本来就用的 ADMIN_PASSWORD 环境变量")


def main():
    ap = argparse.ArgumentParser(description="dy.foxnan.com 后台管理")
    sub = ap.add_subparsers(dest="cmd", required=True)

    g = sub.add_parser("gen-code", help="生成邀请码")
    g.add_argument("--quota", type=int, default=100)
    g.add_argument("--count", type=int, default=1)
    g.add_argument("--note", default="")
    g.add_argument("--order-id", default="")
    g.add_argument("--expire-days", type=int, default=365, help="0 表示长期有效")
    g.set_defaults(func=cmd_gen_code)

    lc = sub.add_parser("list-codes")
    lc.add_argument("--limit", type=int, default=20)
    lc.set_defaults(func=cmd_list_codes)

    lu = sub.add_parser("list-users")
    lu.add_argument("--limit", type=int, default=20)
    lu.set_defaults(func=cmd_list_users)

    gr = sub.add_parser("grant", help="给用户加额度")
    gr.add_argument("--email", required=True)
    gr.add_argument("--quota", type=int, default=100)
    gr.set_defaults(func=cmd_grant)

    for name, fn in (("ban", cmd_ban), ("unban", lambda a: cmd_ban(a, "active"))):
        b = sub.add_parser(name)
        b.add_argument("--email", required=True)
        b.set_defaults(func=fn)

    st = sub.add_parser("stats")
    st.set_defaults(func=cmd_stats)

    ra = sub.add_parser("reset-admin-password", help="忘记后台密码时清除自定义密码")
    ra.set_defaults(func=cmd_reset_admin)

    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
