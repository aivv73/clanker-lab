//! Durable work records. SQLite commits precede acceptance, delivery and execution.
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub struct Store(Connection, #[allow(dead_code)] std::fs::File);
impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("runtime.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("state_directory_in_use".into());
        }
        let db = Connection::open(directory.join("history.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, workspace TEXT, history TEXT NOT NULL DEFAULT '[]');
          CREATE TABLE IF NOT EXISTS commands(id TEXT PRIMARY KEY, command TEXT NOT NULL, acceptance TEXT NOT NULL, result TEXT, session TEXT, turn TEXT UNIQUE);
          CREATE TABLE IF NOT EXISTS events(session TEXT NOT NULL, seq INTEGER NOT NULL, frame TEXT NOT NULL, PRIMARY KEY(session,seq));
          CREATE TABLE IF NOT EXISTS command_processes(id TEXT PRIMARY KEY, session TEXT NOT NULL, turn TEXT NOT NULL, invocation TEXT NOT NULL, result TEXT);
          CREATE TABLE IF NOT EXISTS actions(id TEXT PRIMARY KEY, session TEXT NOT NULL, turn TEXT NOT NULL, intent TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('intent','result','unknown')), result TEXT);
          CREATE TABLE IF NOT EXISTS terminal_processing(session TEXT NOT NULL, turn TEXT PRIMARY KEY, state TEXT NOT NULL CHECK(state IN ('pending','done')));
        ")?;
        Ok(Self(db, lock))
    }
    pub fn workspace(&self, session: &str) -> Result<Option<String>> {
        Ok(self.0.query_row(
            "SELECT workspace FROM sessions WHERE id=?",
            [session],
            |r| r.get(0),
        )?)
    }
    pub fn sessions(&self) -> Result<Vec<String>> {
        let mut stmt = self.0.prepare("SELECT id FROM sessions ORDER BY id")?;
        Ok(stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn blocked(&self, session: &str) -> Result<bool> {
        Ok(self.0.query_row("SELECT EXISTS(SELECT 1 FROM commands WHERE session=? AND json_extract(result,'$.error')='interrupted_outcome_unknown') OR EXISTS(SELECT 1 FROM actions WHERE session=? AND state='unknown')",params![session,session],|r|r.get(0))?)
    }
    pub fn history(&self, session: &str) -> Result<Vec<Value>> {
        let raw: Option<String> = self
            .0
            .query_row("SELECT history FROM sessions WHERE id=?", [session], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(serde_json::from_str(raw.as_deref().unwrap_or("[]"))?)
    }
    pub fn session(&self, session: &str, workspace: Option<&str>, directory: &Path) -> Result<()> {
        if let Some(workspace) = workspace {
            let workspace = std::fs::canonicalize(workspace)?;
            let directory = std::fs::canonicalize(directory)?;
            if directory.starts_with(&workspace) {
                return Err("state_directory_inside_workspace".into());
            }
            let old: Option<String> = self
                .0
                .query_row(
                    "SELECT workspace FROM sessions WHERE id=?",
                    [session],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let workspace = workspace.to_str().ok_or("invalid_workspace")?;
            if old.as_deref().is_some_and(|old| old != workspace) {
                return Err("session_workspace_conflict".into());
            }
            self.0.execute("INSERT INTO sessions(id,workspace) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET workspace=excluded.workspace", params![session,workspace])?;
        } else {
            self.0
                .execute("INSERT OR IGNORE INTO sessions(id) VALUES (?)", [session])?;
        }
        Ok(())
    }
    pub fn receipt(&self, id: &str) -> Result<Option<(Value, Value, Option<Value>)>> {
        let row: Option<(String, String, Option<String>)> = self
            .0
            .query_row(
                "SELECT command,acceptance,result FROM commands WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(c, a, r)| {
            Ok((
                serde_json::from_str(&c)?,
                serde_json::from_str(&a)?,
                r.map(|r| serde_json::from_str(&r)).transpose()?,
            ))
        })
        .transpose()
    }
    pub fn turn_exists(&self, turn: &str) -> Result<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM commands WHERE turn=?)",
            [turn],
            |r| r.get(0),
        )?)
    }
    pub fn accept(&mut self, command: &Value, acceptance: &mut Value) -> Result<()> {
        let tx = self.0.transaction()?;
        let session = command["session_id"].as_str();
        let turn = command["turn_id"].as_str();
        if let Some(session) = session.filter(|_| command["op"] == "turn") {
            append(&tx, session, acceptance)?;
        }
        tx.execute(
            "INSERT INTO commands(id,command,acceptance,session,turn) VALUES (?,?,?,?,?)",
            params![
                command["request_id"].as_str(),
                command.to_string(),
                acceptance.to_string(),
                session,
                if command["op"] == "turn" { turn } else { None }
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn event(&mut self, frame: &mut Value) -> Result<()> {
        let tx = self.0.transaction()?;
        let session = frame["session_id"]
            .as_str()
            .ok_or("missing_session")?
            .to_owned();
        append(&tx, &session, frame)?;
        tx.commit()?;
        Ok(())
    }
    pub fn finish(&mut self, frame: &mut Value, history: Option<&[Value]>) -> Result<()> {
        let tx = self.0.transaction()?;
        let session = frame["session_id"]
            .as_str()
            .ok_or("missing_session")?
            .to_owned();
        if let Some(history) = history {
            tx.execute(
                "UPDATE sessions SET history=? WHERE id=?",
                params![serde_json::to_string(history)?, session],
            )?;
        }
        append(&tx, &session, frame)?;
        tx.execute(
            "UPDATE commands SET result=? WHERE id=? AND result IS NULL",
            params![frame.to_string(), frame["request_id"].as_str()],
        )?;
        tx.execute(
            "INSERT INTO terminal_processing(session,turn,state) VALUES (?,?,'pending')",
            params![session, frame["turn_id"].as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn events(&self, session: &str, after: u64) -> Result<Vec<Value>> {
        let head: i64 = self.0.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE session=?",
            [session],
            |r| r.get(0),
        )?;
        if i64::try_from(after)? > head {
            return Err("event_cursor_ahead".into());
        }
        let mut stmt = self
            .0
            .prepare("SELECT frame FROM events WHERE session=? AND seq>? ORDER BY seq")?;
        let rows = stmt.query_map(params![session, i64::try_from(after)?], |r| {
            r.get::<_, String>(0)
        })?;
        let events: Vec<Value> = rows
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect::<Result<_>>()?;
        for (offset, event) in events.iter().enumerate() {
            if event["seq"].as_u64() != Some(after + offset as u64 + 1) {
                return Err("event_gap".into());
            }
        }
        if after + events.len() as u64 != head as u64 {
            return Err("event_gap".into());
        }
        Ok(events)
    }
    pub fn completed(&self, session: &str) -> Result<Vec<Value>> {
        let mut stmt = self.0.prepare(
            "SELECT result FROM commands WHERE session=? AND result IS NOT NULL ORDER BY rowid",
        )?;
        let rows = stmt.query_map([session], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    /// Restart never re-executes accepted work or claims external cleanup.
    pub fn recover(&mut self, runtime: &str) -> Result<()> {
        let commands: Vec<Value> = {
            let mut stmt = self.0.prepare(
                "SELECT command FROM commands WHERE turn IS NOT NULL AND result IS NULL",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|r| Ok(serde_json::from_str(&r?)?))
                .collect::<Result<_>>()?
        };
        self.0.execute(
            "UPDATE actions SET state='unknown' WHERE state='intent'",
            [],
        )?;
        for c in commands {
            let mut frame = json!({"version":1,"runtime_id":runtime,"request_id":c["request_id"],"session_id":c["session_id"],"turn_id":c["turn_id"],"status":"failed","error":"interrupted_outcome_unknown"});
            self.finish(&mut frame, None)?;
        }
        Ok(())
    }
    pub fn action_intent(&self, id: &str, session: &str, turn: &str, intent: &Value) -> Result<()> {
        self.0.execute(
            "INSERT INTO actions(id,session,turn,intent,state) VALUES (?,?,?,?,'intent')",
            params![id, session, turn, intent.to_string()],
        )?;
        Ok(())
    }
    pub fn action_result(&self, id: &str, result: &Value) -> Result<()> {
        if self.0.execute(
            "UPDATE actions SET state='result',result=? WHERE id=? AND state='intent'",
            params![result.to_string(), id],
        )? != 1
        {
            return Err("action_state_conflict".into());
        }
        Ok(())
    }
    pub fn terminal_done(&self, turn: &str) -> Result<()> {
        if self.0.execute(
            "UPDATE terminal_processing SET state='done' WHERE turn=?",
            [turn],
        )? != 1
        {
            return Err("terminal_missing".into());
        }
        Ok(())
    }
}
fn append(tx: &rusqlite::Transaction<'_>, session: &str, frame: &mut Value) -> Result<()> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE session=?",
        [session],
        |r| r.get(0),
    )?;
    frame["seq"] = json!(seq);
    tx.execute(
        "INSERT INTO events(session,seq,frame) VALUES (?,?,?)",
        params![session, seq, frame.to_string()],
    )?;
    Ok(())
}
