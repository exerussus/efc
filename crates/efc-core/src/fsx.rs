//! Файловые операции канала: атомарная запись, чтение JSON, перенос с повторами, корзина вместо удаления, id.

use crate::trace;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Сколько раз повторять rename, который Windows не пустила (файл открыт антивирусом, индексатором, читателем).
const RETRIES: u32 = 10;

/// Допустимый id: строчные латинские буквы, цифры, `_`, `-`; 1–64 символа.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        && !reserved_on_windows(id)
}

/// Имена устройств Windows: файл `con.json` там не создать.
fn reserved_on_windows(id: &str) -> bool {
    matches!(id, "con" | "prn" | "aux" | "nul")
        || ((id.starts_with("com") || id.starts_with("lpt")) && id.len() == 4 && id.as_bytes()[3].is_ascii_digit())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn noise() -> u64 {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    // splitmix64 — разбросать время, pid и счётчик по битам
    let mut z = n ^ ((std::process::id() as u64) << 32) ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Новый id вида `ГГГГММДД-ччммсс-ммм-<6 hex>` (UTC): сортируется по времени и уникален.
pub fn new_id() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let (y, mo, da, h, mi, s) = crate::time::parts(d.as_secs() as i64);
    format!("{y:04}{mo:02}{da:02}-{h:02}{mi:02}{s:02}-{:03}-{:06x}", d.subsec_millis(), noise() & 0xff_ffff)
}

/// Записать файл атомарно: временный рядом → rename поверх (с повторами).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!("{name}.tmp-{:08x}", noise() as u32));
    let created = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()
    })();
    if let Err(e) = created {
        trace::event("fs.write", json!({ "path": path, "ok": false, "stage": "tmp", "err": trace::io_err(&e) }));
        return Err(e);
    }
    match rename_retry(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            trace::event("fs.write", json!({ "path": path, "ok": false, "stage": "rename", "err": trace::io_err(&e) }));
            Err(e)
        }
    }
}

/// Записать значение как JSON атомарно.
pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let mut text = serde_json::to_vec_pretty(value).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    text.push(b'\n');
    write_atomic(path, &text)
}

/// Переименовать с повторами: на Windows rename поверх открытого файла иногда не проходит с первого раза.
pub fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 0;
    let mut errors: Vec<serde_json::Value> = Vec::new();
    let started = std::time::Instant::now();
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => {
                if !errors.is_empty() {
                    trace::event("fs.rename", json!({ "from": from, "to": to, "ok": true, "attempts": attempt + 1, "errors": errors, "tookMs": trace::ms_since(started) }));
                }
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Err(e),
            // на Windows блокировка файла приходит как ошибка 5/32/33 без своей категории — повторять всё, кроме «нет файла»
            Err(e) if attempt < RETRIES => {
                if trace::enabled() {
                    errors.push(trace::io_err(&e));
                }
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20 + 5 * attempt as u64));
            }
            Err(e) => {
                if trace::enabled() {
                    errors.push(trace::io_err(&e));
                    trace::event("fs.rename", json!({ "from": from, "to": to, "ok": false, "attempts": attempt + 1, "errors": errors, "tookMs": trace::ms_since(started) }));
                }
                return Err(e);
            }
        }
    }
}

/// Прочитать JSON-файл: `None`, если файла нет или он не разбирается. BOM пропускается.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    parse_json(&bytes).ok()
}

/// Разобрать JSON с возможным BOM.
pub fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    serde_json::from_slice(b)
}

/// Файлы `*.json` папки (без скрытых), отсортированные по имени.
pub fn list_json(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !n.starts_with('.') && n.ends_with(".json") && p.is_file()
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Имя файла без `.json`.
pub fn stem(path: &Path) -> String {
    let n = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    n.strip_suffix(".json").unwrap_or(n).to_string()
}

/// Возраст файла по mtime, с (0 — не прочитать).
pub fn age(path: &Path) -> f64 {
    std::fs::metadata(path).and_then(|m| m.modified()).map(crate::time::age_secs).unwrap_or(0.0)
}

/// Чем закончилась уборка файла клиентом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposed {
    /// Удалён.
    Removed,
    /// Удалять нельзя — перенесён в корзину.
    Trashed,
    /// Файла уже нет.
    Gone,
    /// Ни удалить, ни перенести.
    Failed,
}

/// Убрать файл, как велит стандарт клиенту: удалить, а нет права — перенести в корзину канала под именем
/// `trash_name`. `allow_delete = false` — сразу в корзину (среды, где удаление запрещено заранее).
pub fn dispose(path: &Path, trash_dir: &Path, trash_name: &str, allow_delete: bool) -> Disposed {
    let mut delete_err = serde_json::Value::Null;
    if allow_delete {
        match std::fs::remove_file(path) {
            Ok(()) => {
                trace::event("fs.dispose", json!({ "path": path, "result": "removed" }));
                return Disposed::Removed;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Disposed::Gone,
            Err(e) => delete_err = trace::io_err(&e),
        }
    }
    let _ = std::fs::create_dir_all(trash_dir);
    let r = match rename_retry(path, &trash_dir.join(trash_name)) {
        Ok(()) => Disposed::Trashed,
        Err(e) if e.kind() == ErrorKind::NotFound => Disposed::Gone,
        Err(_) => Disposed::Failed,
    };
    trace::event("fs.dispose", json!({ "path": path, "result": format!("{r:?}").to_lowercase(), "deleteError": delete_err }));
    r
}

/// SHA-256 файла строкой hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let a = new_id();
        let b = new_id();
        assert!(valid_id(&a), "{a}");
        assert_ne!(a, b);
        assert_eq!(a.len(), 26);
        assert!(!valid_id("Bad"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_id("con") && !valid_id("com1") && !valid_id("lpt9") && !valid_id("nul"));
        assert!(valid_id("com") && valid_id("console") && valid_id("com12"));
    }
}
