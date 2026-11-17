//! 账号与额度存储（SQLite）。
//!
//! 只做四件事：注册（消费邀请码）、登录会话、按次扣额度、内网发码。
//! 用 rusqlite + 单连接 + Mutex：这台机器是低并发工具站，够用且省事。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  email         TEXT NOT NULL UNIQUE,
  pass_hash     TEXT NOT NULL,
  balance       INTEGER NOT NULL DEFAULT 0,
  status        TEXT NOT NULL DEFAULT 'active',
  created_at    TEXT NOT NULL,
  last_login_at TEXT
);
CREATE TABLE IF NOT EXISTS invite_codes (
  code        TEXT PRIMARY KEY,
  quota       INTEGER NOT NULL,
  note        TEXT NOT NULL DEFAULT '',
  order_id    TEXT NOT NULL DEFAULT '',
  expire_at   TEXT,
  created_at  TEXT NOT NULL,
  redeemed_by INTEGER,
  redeemed_at TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
  token      TEXT PRIMARY KEY,
  user_id    INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  ip         TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS usage_log (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id       INTEGER NOT NULL,
  kind          TEXT NOT NULL,
  cost          INTEGER NOT NULL,
  balance_after INTEGER NOT NULL,
  created_at    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  k          TEXT PRIMARY KEY,
  v          TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_usage_user ON usage_log(user_id, created_at);
"#;

#[derive(Debug)]
pub enum DbError {
    /// 可以原样返回给用户的中文提示
    Msg(String),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sql(e)
    }
}

impl DbError {
    pub fn text(&self) -> String {
        match self {
            DbError::Msg(m) => m.clone(),
            DbError::Sql(e) => format!("数据库错误：{e}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub balance: i64,
    pub status: String,
}

pub struct Db {
    conn: Mutex<Connection>,
}

fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn expires_str(days: i64) -> String {
    (chrono::Local::now() + chrono::Duration::days(days))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

impl Db {
    pub fn open(path: &str) -> Result<Self, DbError> {
        if let Some(dir) = Path::new(path).parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).ok();
            }
        }
        let conn = Connection::open(path)?;
        // WAL：后台脚本（python）和应用同时读写不会互相锁死
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// 注册：同一个事务里「核销邀请码 + 建用户 + 入账」，任一步失败全部回滚。
    pub fn register(&self, email: &str, pass_hash: &str, code: &str) -> Result<User, DbError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let now = now_str();

        let row: Option<(i64, Option<String>, Option<i64>)> = tx
            .query_row(
                "SELECT quota, expire_at, redeemed_by FROM invite_codes WHERE code = ?1",
                params![code],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let Some((quota, expire_at, redeemed_by)) = row else {
            return Err(DbError::Msg("邀请码不存在，请检查是否输错".into()));
        };
        if redeemed_by.is_some() {
            return Err(DbError::Msg("这个邀请码已经被使用过了".into()));
        }
        if let Some(exp) = expire_at {
            if exp.as_str() < now.as_str() {
                return Err(DbError::Msg("邀请码已过期".into()));
            }
        }

        let exists: Option<i64> = tx
            .query_row("SELECT id FROM users WHERE email = ?1", params![email], |r| r.get(0))
            .optional()?;
        if exists.is_some() {
            return Err(DbError::Msg("这个邮箱已经注册过了，直接登录即可".into()));
        }

        tx.execute(
            "INSERT INTO users (email, pass_hash, balance, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![email, pass_hash, quota, now],
        )?;
        let uid = tx.last_insert_rowid();
        tx.execute(
            "UPDATE invite_codes SET redeemed_by = ?1, redeemed_at = ?2 WHERE code = ?3",
            params![uid, now, code],
        )?;
        tx.execute(
            "INSERT INTO usage_log (user_id, kind, cost, balance_after, created_at) VALUES (?1, 'register', 0, ?2, ?3)",
            params![uid, quota, now],
        )?;
        tx.commit()?;

        Ok(User { id: uid, email: email.to_string(), balance: quota, status: "active".into() })
    }

    pub fn find_login(&self, email: &str) -> Option<(i64, String, String, i64)> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id, pass_hash, status, balance FROM users WHERE email = ?1",
            params![email],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn create_session(&self, user_id: i64, token: &str, ip: &str) -> Result<(), DbError> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sessions (token, user_id, created_at, expires_at, ip) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token, user_id, now_str(), expires_str(30), ip],
        )?;
        c.execute("UPDATE users SET last_login_at = ?1 WHERE id = ?2", params![now_str(), user_id])?;
        Ok(())
    }

    pub fn user_by_token(&self, token: &str) -> Option<User> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT u.id, u.email, u.balance, u.status
               FROM sessions s JOIN users u ON u.id = s.user_id
              WHERE s.token = ?1 AND s.expires_at > ?2",
            params![token, now_str()],
            |r| {
                Ok(User {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    balance: r.get(2)?,
                    status: r.get(3)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn drop_session(&self, token: &str) {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM sessions WHERE token = ?1", params![token]).ok();
    }

    /// 扣额度。返回扣完后的余额；不够则返回余额本身（由调用方拼提示）。
    pub fn charge(&self, user_id: i64, kind: &str, cost: i64) -> Result<Result<i64, i64>, DbError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let balance: i64 = tx.query_row(
            "SELECT balance FROM users WHERE id = ?1",
            params![user_id],
            |r| r.get(0),
        )?;
        if balance < cost {
            return Ok(Err(balance));
        }
        let after = balance - cost;
        tx.execute("UPDATE users SET balance = ?1 WHERE id = ?2", params![after, user_id])?;
        tx.execute(
            "INSERT INTO usage_log (user_id, kind, cost, balance_after, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user_id, kind, cost, after, now_str()],
        )?;
        tx.commit()?;
        Ok(Ok(after))
    }

    pub fn balance(&self, user_id: i64) -> i64 {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT balance FROM users WHERE id = ?1", params![user_id], |r| r.get(0))
            .unwrap_or(0)
    }

    /// 老用户续费：核销一个码，额度加到现有余额上。
    pub fn redeem(&self, user_id: i64, code: &str) -> Result<i64, DbError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let now = now_str();

        let row: Option<(i64, Option<String>, Option<i64>)> = tx
            .query_row(
                "SELECT quota, expire_at, redeemed_by FROM invite_codes WHERE code = ?1",
                params![code],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((quota, expire_at, redeemed_by)) = row else {
            return Err(DbError::Msg("邀请码不存在，请检查是否输错".into()));
        };
        if redeemed_by.is_some() {
            return Err(DbError::Msg("这个邀请码已经被使用过了".into()));
        }
        if let Some(exp) = expire_at {
            if exp.as_str() < now.as_str() {
                return Err(DbError::Msg("邀请码已过期".into()));
            }
        }

        tx.execute(
            "UPDATE invite_codes SET redeemed_by = ?1, redeemed_at = ?2 WHERE code = ?3",
            params![user_id, now, code],
        )?;
        tx.execute("UPDATE users SET balance = balance + ?1 WHERE id = ?2", params![quota, user_id])?;
        let after: i64 = tx.query_row(
            "SELECT balance FROM users WHERE id = ?1",
            params![user_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO usage_log (user_id, kind, cost, balance_after, created_at) VALUES (?1, 'redeem', -?2, ?3, ?4)",
            params![user_id, quota, after, now],
        )?;
        tx.commit()?;
        Ok(after)
    }

    /// 发一个码（内网接口 / 后台脚本共用）。码形如 DY-XXXX-XXXX-XXXX。
    pub fn issue_code(&self, quota: i64, note: &str, order_id: &str, expire_days: i64) -> Result<String, DbError> {
        let c = self.conn.lock().unwrap();
        for _ in 0..8 {
            let code = gen_code();
            let expire = if expire_days > 0 {
                Some(expires_str(expire_days))
            } else {
                None
            };
            let ok = c.execute(
                "INSERT OR IGNORE INTO invite_codes (code, quota, note, order_id, expire_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![code, quota, note, order_id, expire, now_str()],
            )?;
            if ok == 1 {
                return Ok(code);
            }
        }
        Err(DbError::Msg("生成邀请码失败，请重试".into()))
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT v FROM settings WHERE k = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), DbError> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO settings (k, v, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v, updated_at = excluded.updated_at",
            params![key, value, now_str()],
        )?;
        Ok(())
    }

    pub fn code_by_order(&self, order_id: &str) -> Option<String> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT code FROM invite_codes WHERE order_id = ?1 AND order_id <> '' LIMIT 1",
            params![order_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }
}

// ---------- 管理后台用的查询 ----------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub users: i64,
    pub new_today: i64,
    pub active_7d: i64,
    pub codes_total: i64,
    pub codes_used: i64,
    pub calls_today: i64,
    pub balance_sum: i64,
    pub cost_total: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRow {
    pub id: i64,
    pub email: String,
    pub balance: i64,
    pub status: String,
    pub created_at: String,
    pub last_login_at: String,
    pub spent: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeRow {
    pub code: String,
    pub quota: i64,
    pub note: String,
    pub order_id: String,
    pub expire_at: String,
    pub created_at: String,
    pub used: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub email: String,
    pub kind: String,
    pub cost: i64,
    pub balance_after: i64,
    pub created_at: String,
}

impl Db {
    pub fn overview(&self) -> Overview {
        let c = self.conn.lock().unwrap();
        let one = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        Overview {
            users: one("SELECT COUNT(*) FROM users"),
            new_today: one("SELECT COUNT(*) FROM users WHERE substr(created_at,1,10) = date('now','localtime')"),
            active_7d: one(
                "SELECT COUNT(DISTINCT user_id) FROM usage_log WHERE created_at >= datetime('now','localtime','-7 days')",
            ),
            codes_total: one("SELECT COUNT(*) FROM invite_codes"),
            codes_used: one("SELECT COUNT(*) FROM invite_codes WHERE redeemed_by IS NOT NULL"),
            calls_today: one(
                "SELECT COUNT(*) FROM usage_log WHERE cost > 0 AND substr(created_at,1,10) = date('now','localtime')",
            ),
            balance_sum: one("SELECT COALESCE(SUM(balance),0) FROM users"),
            cost_total: one("SELECT COALESCE(SUM(cost),0) FROM usage_log WHERE cost > 0"),
        }
    }

    pub fn list_users(&self, q: &str, limit: i64, offset: i64) -> Vec<UserRow> {
        let c = self.conn.lock().unwrap();
        let like = format!("%{}%", q.trim());
        let mut stmt = c
            .prepare(
                "SELECT u.id, u.email, u.balance, u.status, u.created_at,
                        COALESCE(u.last_login_at,''),
                        COALESCE((SELECT SUM(cost) FROM usage_log l WHERE l.user_id = u.id AND l.cost > 0), 0)
                   FROM users u
                  WHERE (?1 = '%%' OR u.email LIKE ?1)
                  ORDER BY u.id DESC LIMIT ?2 OFFSET ?3",
            )
            .unwrap();
        stmt.query_map(params![like, limit, offset], |r| {
            Ok(UserRow {
                id: r.get(0)?,
                email: r.get(1)?,
                balance: r.get(2)?,
                status: r.get(3)?,
                created_at: r.get(4)?,
                last_login_at: r.get(5)?,
                spent: r.get(6)?,
            })
        })
        .map(|it| it.filter_map(|x| x.ok()).collect())
        .unwrap_or_default()
    }

    pub fn count_users(&self, q: &str) -> i64 {
        let c = self.conn.lock().unwrap();
        let like = format!("%{}%", q.trim());
        c.query_row("SELECT COUNT(*) FROM users WHERE ?1 = '%%' OR email LIKE ?1", params![like], |r| r.get(0))
            .unwrap_or(0)
    }

    /// 后台加减额度：正数加、负数扣（扣到 0 为止，不会变负）
    pub fn adjust_balance(&self, email: &str, delta: i64, note: &str) -> Result<(String, i64), DbError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let row: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT id, email, balance FROM users WHERE email = ?1",
                params![email],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((uid, mail, bal)) = row else {
            return Err(DbError::Msg("没有这个用户".into()));
        };
        let after = (bal + delta).max(0);
        tx.execute("UPDATE users SET balance = ?1 WHERE id = ?2", params![after, uid])?;
        tx.execute(
            "INSERT INTO usage_log (user_id, kind, cost, balance_after, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uid, format!("admin:{note}"), -delta, after, now_str()],
        )?;
        tx.commit()?;
        Ok((mail, after))
    }

    pub fn set_status(&self, email: &str, status: &str) -> Result<(), DbError> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE users SET status = ?1 WHERE email = ?2",
            params![status, email],
        )?;
        if n == 0 {
            return Err(DbError::Msg("没有这个用户".into()));
        }
        Ok(())
    }

    pub fn set_password(&self, email: &str, hash: &str) -> Result<(), DbError> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE users SET pass_hash = ?1 WHERE email = ?2",
            params![hash, email],
        )?;
        if n == 0 {
            return Err(DbError::Msg("没有这个用户".into()));
        }
        Ok(())
    }

    pub fn list_codes(&self, limit: i64, offset: i64) -> Vec<CodeRow> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c
            .prepare(
                "SELECT code, quota, note, order_id, COALESCE(expire_at,''), created_at,
                        CASE WHEN redeemed_by IS NULL THEN 0 ELSE 1 END
                   FROM invite_codes ORDER BY created_at DESC, code LIMIT ?1 OFFSET ?2",
            )
            .unwrap();
        stmt.query_map(params![limit, offset], |r| {
            Ok(CodeRow {
                code: r.get(0)?,
                quota: r.get(1)?,
                note: r.get(2)?,
                order_id: r.get(3)?,
                expire_at: r.get(4)?,
                created_at: r.get(5)?,
                used: r.get::<_, i64>(6)? == 1,
            })
        })
        .map(|it| it.filter_map(|x| x.ok()).collect())
        .unwrap_or_default()
    }

    /// 只允许删没用过的码，避免把已经卖出去的码删掉。
    pub fn delete_code(&self, code: &str) -> Result<(), DbError> {
        let c = self.conn.lock().unwrap();
        let used: Option<i64> = c
            .query_row(
                "SELECT CASE WHEN redeemed_by IS NULL THEN 0 ELSE 1 END FROM invite_codes WHERE code = ?1",
                params![code],
                |r| r.get(0),
            )
            .optional()?;
        match used {
            None => Err(DbError::Msg("没有这个码".into())),
            Some(1) => Err(DbError::Msg("这个码已经被用了，不能删".into())),
            Some(_) => {
                c.execute("DELETE FROM invite_codes WHERE code = ?1", params![code])?;
                Ok(())
            }
        }
    }

    pub fn recent_usage(&self, limit: i64) -> Vec<UsageRow> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c
            .prepare(
                "SELECT COALESCE(u.email,'-'), l.kind, l.cost, l.balance_after, l.created_at
                   FROM usage_log l LEFT JOIN users u ON u.id = l.user_id
                  ORDER BY l.id DESC LIMIT ?1",
            )
            .unwrap();
        stmt.query_map(params![limit], |r| {
            Ok(UsageRow {
                email: r.get(0)?,
                kind: r.get(1)?,
                cost: r.get(2)?,
                balance_after: r.get(3)?,
                created_at: r.get(4)?,
            })
        })
        .map(|it| it.filter_map(|x| x.ok()).collect())
        .unwrap_or_default()
    }
}

fn gen_code() -> String {
    use rand::RngCore;
    // 去掉容易看错的 0/O/1/I/L
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut buf = [0u8; 12];
    rand::rng().fill_bytes(&mut buf);
    let body: String = buf
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    format!("DY-{}-{}-{}", &body[0..4], &body[4..8], &body[8..12])
}

/// 32 字节随机 token 的十六进制串（会话 id）
pub fn gen_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 邮箱只做最基本的形状校验：有 @、有域名点、长度合理。
pub fn valid_email(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 5 || s.len() > 120 || s.contains(' ') {
        return false;
    }
    match s.split_once('@') {
        Some((a, b)) => !a.is_empty() && b.contains('.') && !b.starts_with('.') && !b.ends_with('.'),
        None => false,
    }
}
