//! Трассировка: каждое действие с файлами канала — строкой JSON в журнал. Для разбора первых запусков на новой
//! машине или ОС: что сервер увидел, когда забрал, сколько раз повторял rename и с каким кодом ошибки ОС.
//!
//! Включение: `EFC_TRACE=1` — журнал в `<канал>/trace/` (виден и клиенту из ВМ); `EFC_TRACE=stderr`;
//! `EFC_TRACE=<файл>`. Пока трассировка выключена, вызовы [`event`] ничего не стоят.
//!
//! Строка: `{"t":"…Z","ms":<от старта процесса>,"pid":…,"side":"server|client","th":"<поток>","ev":"…", …поля}`.

use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static ON: AtomicBool = AtomicBool::new(false);

struct Sink {
    out: Out,
    side: &'static str,
}

enum Out {
    File(File, PathBuf),
    Stderr,
}

fn sink() -> &'static Mutex<Option<Sink>> {
    static S: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn start() -> Instant {
    static T: OnceLock<Instant> = OnceLock::new();
    *T.get_or_init(Instant::now)
}

/// Трассировка включена.
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Что просит `EFC_TRACE`: `None` — выключено.
pub fn requested() -> Option<String> {
    std::env::var("EFC_TRACE").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// Включить по `EFC_TRACE` для канала `channel` (сторона `side`). Возвращает, куда пишется журнал.
pub fn init_from_env(channel: &Path, side: &'static str) -> Option<String> {
    let v = requested()?;
    if enabled() {
        return current();
    }
    let r = if v.eq_ignore_ascii_case("stderr") {
        enable_stderr(side);
        Some("stderr".to_string())
    } else if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") {
        let day = crate::time::now_iso()[..10].replace('-', "");
        enable_file(&channel.join("trace").join(format!("{side}-{day}.jsonl")), side).ok().map(|p| p.display().to_string())
    } else {
        enable_file(Path::new(&v), side).ok().map(|p| p.display().to_string())
    };
    if r.is_some() {
        event("trace.on", json!({ "efc": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "channel": channel.display().to_string() }));
    }
    r
}

/// Писать журнал в файл (дописывая).
pub fn enable_file(path: &Path, side: &'static str) -> std::io::Result<PathBuf> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    let _ = start();
    *sink().lock().unwrap() = Some(Sink { out: Out::File(f, path.to_path_buf()), side });
    ON.store(true, Ordering::SeqCst);
    Ok(path.to_path_buf())
}

/// Писать журнал в stderr.
pub fn enable_stderr(side: &'static str) {
    let _ = start();
    *sink().lock().unwrap() = Some(Sink { out: Out::Stderr, side });
    ON.store(true, Ordering::SeqCst);
}

/// Выключить.
pub fn disable() {
    ON.store(false, Ordering::SeqCst);
    *sink().lock().unwrap() = None;
}

/// Куда пишется журнал.
pub fn current() -> Option<String> {
    sink().lock().unwrap().as_ref().map(|s| match &s.out {
        Out::File(_, p) => p.display().to_string(),
        Out::Stderr => "stderr".into(),
    })
}

/// Записать событие. `fields` — объект JSON (или `Value::Null`).
pub fn event(ev: &str, fields: Value) {
    if !enabled() {
        return;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut m = Map::new();
    let (y, mo, d, h, mi, s) = crate::time::parts(now.as_secs() as i64);
    m.insert("t".into(), Value::String(format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", now.subsec_millis())));
    m.insert("ms".into(), json!((start().elapsed().as_secs_f64() * 10_000.0).round() / 10.0));
    m.insert("pid".into(), json!(std::process::id()));
    let th = std::thread::current();
    m.insert("th".into(), Value::String(th.name().unwrap_or("?").to_string()));
    m.insert("ev".into(), Value::String(ev.to_string()));
    if let Value::Object(f) = fields {
        m.extend(f);
    }
    let mut g = sink().lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    m.insert("side".into(), Value::String(s.side.into()));
    let mut line = serde_json::to_string(&Value::Object(m)).unwrap_or_default();
    line.push('\n');
    // одной записью: строки разных процессов в общем файле не перемешиваются
    match &mut s.out {
        Out::File(f, _) => {
            let _ = f.write_all(line.as_bytes());
        }
        Out::Stderr => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

/// Ошибка ввода-вывода для журнала: категория, код ОС (на Windows 5 — доступ, 32/33 — файл занят) и текст.
pub fn io_err(e: &std::io::Error) -> Value {
    json!({ "kind": format!("{:?}", e.kind()), "os": e.raw_os_error(), "msg": e.to_string() })
}

/// Миллисекунды с момента `t` (для полей длительности).
pub fn ms_since(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 10_000.0).round() / 10.0
}
