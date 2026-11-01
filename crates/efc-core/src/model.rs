//! Файлы канала efc/1 (SPEC, разделы 2, 4, 5, 8, 10, 11) и раскладка папок.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Идентификатор протокола.
pub const PROTOCOL: &str = "efc/1";
/// Старшая версия формата файлов.
pub const V: u32 = 1;

/// Коды выхода (SPEC, раздел 13).
pub mod exit {
    pub const OK: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const UNKNOWN_COMMAND: i32 = 3;
    pub const CRASHED: i32 = 4;
    pub const TIMEOUT: i32 = 5;
    pub const BUSY: i32 = 6;
    pub const CANCELLED: i32 = 7;
    pub const DENIED: i32 = 8;
    pub const NO_SERVER: i32 = 10;
    pub const AMBIGUOUS: i32 = 11;
    pub const WARD_SILENT: i32 = 12;
    pub const SERVER_SILENT: i32 = 13;
}

/// Стандартные машинные причины.
pub mod reason {
    pub const EXPIRED: &str = "expired";
    pub const CANCELLED: &str = "cancelled";
    pub const SERVER_RESTARTED: &str = "server-restarted";
    pub const BUSY: &str = "busy";
    pub const NO_APPROVAL: &str = "no-approval";
    pub const BAD_REQUEST: &str = "bad-request";
    pub const UNKNOWN_COMMAND: &str = "unknown-command";
    pub const FOREIGN_INSTANCE: &str = "foreign-instance";
    pub const TOO_LARGE: &str = "too-large";
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn v1() -> u32 {
    V
}

/// Путь в запросе или итоге: строкой (относительно `project`) или `{root, path}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PathRef {
    Plain(String),
    Rooted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abs: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
}

impl PathRef {
    /// Ссылка на путь под корнем.
    pub fn rooted(root: &str, path: &str) -> Self {
        PathRef::Rooted { root: Some(root.into()), path: path.into(), abs: None, kind: None }
    }

    pub fn root(&self) -> Option<&str> {
        match self {
            PathRef::Plain(_) => None,
            PathRef::Rooted { root, .. } => root.as_deref(),
        }
    }

    pub fn path(&self) -> &str {
        match self {
            PathRef::Plain(p) => p,
            PathRef::Rooted { path, .. } => path,
        }
    }
}

/// Проверка результата шага (SPEC 5.2.1).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Expect {
    /// Ожидаемый код (по умолчанию 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// Подстрока, которая должна быть в выводе.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Подстрока, которой в выводе быть не должно.
    #[serde(default, rename = "notOutput", skip_serializing_if = "Option::is_none")]
    pub not_output: Option<String>,
    /// Поля `data` (путь через точку) и их значения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Map<String, Value>>,
}

/// Одна команда пакета (шаг).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Op {
    /// Имя шага для подстановок `${id.поле}` (по умолчанию `s1`, `s2`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
    /// Выполнять и после ошибки или отмены (уборка).
    #[serde(default, skip_serializing_if = "is_false")]
    pub always: bool,
    /// `stop` | `continue` — для этого шага (иначе — как у пакета).
    #[serde(default, rename = "onError", skip_serializing_if = "Option::is_none")]
    pub on_error: Option<String>,
    /// Предел шага, с.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

/// Запрос — `requests/<id>.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "v1")]
    pub v: u32,
    /// Пустой — клиент подставит новый (в файле пакета id не нужен); сервер пустой id отклоняет.
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<Vec<Op>>,
    #[serde(default, rename = "onError", skip_serializing_if = "Option::is_none")]
    pub on_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<f64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
    /// Пока вызов идёт, чужие меняющие вызовы получают код 6 (если сервер объявил `lock`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub lock: bool,
}

/// Что нужно, чтобы вызов пошёл дальше.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Asks {
    pub who: String,
    pub text: String,
}

/// Принятый вызов — `active/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Active {
    #[serde(default = "v1")]
    pub v: u32,
    pub id: String,
    /// `running` | `waiting`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asks: Option<Asks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

/// Итог элемента пакета.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Item {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `ok` | `failed` | `skipped` | `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub ok: bool,
    #[serde(rename = "exitCode")]
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    #[serde(default, rename = "expectFailed", skip_serializing_if = "Vec::is_empty")]
    pub expect_failed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

/// Итог — `done/<id>.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Done {
    #[serde(default = "v1")]
    pub v: u32,
    pub id: String,
    #[serde(default = "done_state")]
    pub state: String,
    pub ok: bool,
    #[serde(rename = "exitCode")]
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub output: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<PathRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<Item>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, rename = "cancelIgnored", skip_serializing_if = "is_false")]
    pub cancel_ignored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
}

fn done_state() -> String {
    "done".into()
}

/// Просьба отменить — `cancel/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cancel {
    #[serde(default = "v1")]
    pub v: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Пульс — `alive.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alive {
    #[serde(default = "v1")]
    pub v: u32,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub beat: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// `ready` | `busy` | `degraded` | `stopping`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub detail: Value,
}

/// Описание команды в паспорте.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CommandInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub usage: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub long: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub idempotent: bool,
    #[serde(default, rename = "readOnly", skip_serializing_if = "is_false")]
    pub read_only: bool,
}

impl CommandInfo {
    pub fn new(name: &str, summary: &str, usage: &str) -> Self {
        CommandInfo { name: name.into(), summary: summary.into(), usage: usage.into(), ..Default::default() }
    }
    pub fn long(mut self) -> Self {
        self.long = true;
        self
    }
    pub fn idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

/// Лимиты канала.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub request_bytes: u64,
    pub output_chars: u64,
    pub default_timeout: f64,
    pub default_keep: f64,
    pub min_keep: f64,
    pub max_keep: f64,
    pub trash_max_age: f64,
    pub artifact_keep: f64,
    pub queue: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            request_bytes: 1 << 20,
            output_chars: 512 * 1024,
            default_timeout: 600.0,
            default_keep: 300.0,
            min_keep: 60.0,
            max_keep: 86_400.0,
            trash_max_age: 86_400.0,
            artifact_keep: 86_400.0,
            queue: 100,
        }
    }
}

/// Клиент, который сервер кладёт в канал.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientFile {
    pub file: String,
    pub sha256: String,
}

/// Паспорт — `channel.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub protocol: String,
    pub instance: String,
    pub server: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub how_to_start: String,
    #[serde(default)]
    pub roots: BTreeMap<String, String>,
    #[serde(default = "default_beat")]
    pub beat: f64,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub subchannels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<ClientFile>,
}

fn default_beat() -> f64 {
    2.0
}

impl Channel {
    /// Старшая версия протокола из поля `protocol` (`efc/1` → 1).
    pub fn major(&self) -> Option<u32> {
        self.protocol.strip_prefix("efc/")?.parse().ok()
    }
}

/// Ожидающее действие — `inbox/<key>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    #[serde(default = "v1")]
    pub v: u32,
    pub key: String,
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggest: Vec<Op>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<PathRef>,
}

/// Запись указателя каналов — `.efc-channels.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexEntry {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// Имя файла указателя каналов.
pub const INDEX_FILE: &str = ".efc-channels.json";

/// Раскладка канала.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }
    pub fn channel_json(&self) -> PathBuf {
        self.root.join("channel.json")
    }
    pub fn alive_json(&self) -> PathBuf {
        self.root.join("alive.json")
    }
    pub fn readme(&self) -> PathBuf {
        self.root.join("README.txt")
    }
    pub fn requests(&self) -> PathBuf {
        self.root.join("requests")
    }
    pub fn active(&self) -> PathBuf {
        self.root.join("active")
    }
    pub fn done(&self) -> PathBuf {
        self.root.join("done")
    }
    pub fn cancel(&self) -> PathBuf {
        self.root.join("cancel")
    }
    pub fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }
    pub fn inbox(&self) -> PathBuf {
        self.root.join("inbox")
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }
    pub fn request(&self, id: &str) -> PathBuf {
        self.requests().join(format!("{id}.json"))
    }
    pub fn active_file(&self, id: &str) -> PathBuf {
        self.active().join(format!("{id}.json"))
    }
    pub fn active_log(&self, id: &str) -> PathBuf {
        self.active().join(format!("{id}.log"))
    }
    pub fn done_file(&self, id: &str) -> PathBuf {
        self.done().join(format!("{id}.json"))
    }
    pub fn cancel_file(&self, id: &str) -> PathBuf {
        self.cancel().join(format!("{id}.json"))
    }
    /// Папки, которые создаёт сервер.
    pub fn dirs(&self) -> Vec<PathBuf> {
        vec![self.requests(), self.active(), self.done(), self.cancel(), self.trash()]
    }

    /// Перевести ссылку из ответа в локальный путь по `roots` паспорта. `None` — ссылка выходит за корень.
    pub fn resolve(&self, channel: Option<&Channel>, r: &PathRef) -> Option<PathBuf> {
        let rel = Path::new(r.path());
        if !safe_relative(rel) {
            return None;
        }
        let root_name = r.root().unwrap_or("project");
        let base = channel
            .and_then(|c| c.roots.get(root_name).or_else(|| if r.root().is_none() { c.roots.get("channel") } else { None }))
            .map(|s| self.root.join(s))
            .unwrap_or_else(|| self.root.clone());
        Some(base.join(rel))
    }
}

/// Путь относительный и без `..`.
pub fn safe_relative(p: &Path) -> bool {
    use std::path::Component;
    !p.as_os_str().is_empty() && p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_examples_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let ch: Channel = crate::fsx::read_json(&dir.join("channel.json")).expect("channel");
        assert_eq!(ch.major(), Some(1));
        let r: Request = crate::fsx::read_json(&dir.join("request-batch.json")).expect("batch");
        assert_eq!(r.batch.unwrap().len(), 2);
        let d: Done = crate::fsx::read_json(&dir.join("result-test-failed.json")).expect("done");
        assert_eq!(d.exit_code, 1);
        assert_eq!(d.artifacts[0].root(), Some("project"));
        let a: Active = crate::fsx::read_json(&dir.join("active-waiting.json")).expect("active");
        assert_eq!(a.asks.unwrap().who, "human");
        let i: InboxItem = crate::fsx::read_json(&dir.join("inbox-item.json")).expect("inbox");
        assert_eq!(i.suggest.len(), 1);
        let idx: Vec<IndexEntry> = crate::fsx::read_json(&dir.join(".efc-channels.json")).expect("index");
        assert_eq!(idx.len(), 2);
        let st: Request = crate::fsx::read_json(&dir.join("request-steps.json")).expect("steps");
        assert!(st.lock && crate::steps::uses_steps(&st));
        assert_eq!(st.batch.as_ref().unwrap()[2].expect.as_ref().unwrap().data.as_ref().unwrap()["rows"], 0);
        let sd: Done = crate::fsx::read_json(&dir.join("result-steps.json")).expect("steps result");
        assert_eq!(sd.items.unwrap()[2].expect_failed.len(), 1);
    }

    #[test]
    fn resolve_paths() {
        let l = Layout::new("/c/.efc");
        let mut ch: Channel = serde_json::from_str(r#"{"protocol":"efc/1","instance":"i","server":"s","version":"1"}"#).unwrap();
        ch.roots.insert("project".into(), "..".into());
        assert_eq!(l.resolve(Some(&ch), &PathRef::rooted("project", "a/b.txt")).unwrap(), Path::new("/c/.efc/../a/b.txt"));
        assert!(l.resolve(Some(&ch), &PathRef::Plain("../etc/passwd".into())).is_none());
        assert!(l.resolve(Some(&ch), &PathRef::Plain("/etc/passwd".into())).is_none());
    }
}
