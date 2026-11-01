//! Отладка первых запусков: `efc selftest` (как ведёт себя файловая система этой машины + сервер и клиент в одном
//! процессе), `efc bench` (задержки вызова с разбивкой), `efc trace` (журнал трассировки канала).

use efc_core::client::{CallOpts, Client, Waited};
use efc_core::model::{exit, CommandInfo, Layout, Op};
use efc_core::server::{Ctx, Handler, Outcome, Server, ServerConfig};
use efc_core::{check, fsx};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

// ====================================================================== bench

/// Итоги замера.
pub struct BenchStats {
    pub n: usize,
    pub failed: usize,
    pub rtt: Vec<f64>,
    pub age: Vec<f64>,
    pub work: Vec<f64>,
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((s.len() - 1) as f64 * p).round() as usize]
}

/// N вызовов `ping` подряд.
pub fn bench(c: &Client, n: usize) -> BenchStats {
    let mut st = BenchStats { n, failed: 0, rtt: vec![], age: vec![], work: vec![] };
    let opts = CallOpts { timeout: Some(30.0), keep: Some(60.0), max_wait: Some(15.0), ..Default::default() };
    for _ in 0..n {
        let t = Instant::now();
        match c.call("ping", &[], Value::Null, &opts) {
            Waited::Done(d) if d.ok => {
                st.rtt.push(t.elapsed().as_secs_f64() * 1000.0);
                st.age.push(d.data.get("ageAtClaimMs").and_then(Value::as_f64).unwrap_or(f64::NAN));
                st.work.push(d.seconds.unwrap_or(0.0) * 1000.0);
            }
            _ => st.failed += 1,
        }
    }
    st
}

pub fn bench_report(st: &BenchStats) -> Vec<String> {
    let mut out = Vec::new();
    if st.rtt.is_empty() {
        out.push(format!("✗ ни один из {} вызовов ping не дошёл до итога", st.n));
        return out;
    }
    let ages: Vec<f64> = st.age.iter().copied().filter(|a| a.is_finite()).collect();
    let rest: Vec<f64> = st.rtt.iter().zip(&st.age).zip(&st.work).map(|((r, a), w)| r - if a.is_finite() { *a } else { 0.0 } - w).collect();
    let line = |name: &str, v: &[f64]| format!("{name:<34} мин {:>5.0}  медиана {:>5.0}  95% {:>5.0}  макс {:>5.0} мс", pct(v, 0.0), pct(v, 0.5), pct(v, 0.95), pct(v, 1.0));
    out.push(format!("вызовов: {}, без итога: {}", st.n, st.failed));
    out.push(line("весь вызов (клиент)", &st.rtt));
    if !ages.is_empty() {
        out.push(line("запрос ждал, пока сервер заберёт", &ages));
    }
    out.push(line("работа сервера", &st.work));
    out.push(line("остальное: запись, доставка итога", &rest));
    let med_age = pct(&ages, 0.5);
    let med_rest = pct(&rest, 0.5);
    if med_age > 150.0 {
        out.push(format!("! сервер поздно видит запросы ({med_age:.0} мс): медленный опрос или общая папка поздно показывает файлы серверу"));
    }
    if med_rest > 200.0 {
        out.push(format!("! итог долго доходит до клиента ({med_rest:.0} мс): задержка общей папки ВМ или медленная запись"));
    }
    if st.failed > 0 {
        out.push("✗ часть вызовов без итога — смотрите efc trace".into());
    }
    out
}

// ====================================================================== selftest

struct Line {
    mark: char,
    name: String,
    detail: String,
}

fn ok(name: &str, detail: impl Into<String>) -> Line {
    Line { mark: '✓', name: name.into(), detail: detail.into() }
}
fn bad(name: &str, detail: impl Into<String>) -> Line {
    Line { mark: '✗', name: name.into(), detail: detail.into() }
}
fn warn(name: &str, detail: impl Into<String>) -> Line {
    Line { mark: '!', name: name.into(), detail: detail.into() }
}
fn info(name: &str, detail: impl Into<String>) -> Line {
    Line { mark: 'i', name: name.into(), detail: detail.into() }
}

fn err_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(c) => format!("{e} (код ОС {c})"),
        None => e.to_string(),
    }
}

/// Только встроенные команды.
struct Bare;

impl Handler for Bare {
    fn commands(&self) -> Vec<CommandInfo> {
        vec![CommandInfo::new("echo", "Повторить аргументы", "echo …")]
    }
    fn run(&self, _ctx: &Ctx, op: &Op) -> Outcome {
        Outcome::ok(op.args.join(" ") + "\n")
    }
}

/// Самопроверка в папке `base` (по умолчанию — текущая). Возвращает код выхода.
pub fn selftest(base: &Path, json_out: bool) -> i32 {
    let work = base.join(format!("efc-selftest-{}", fsx::new_id()));
    let mut lines: Vec<Line> = Vec::new();
    if let Err(e) = std::fs::create_dir_all(&work) {
        eprintln!("✗ не создать {}: {}", work.display(), err_text(&e));
        return exit::FAILED;
    }
    if !json_out {
        println!("efc {} — самопроверка: {} {}, папка {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH, work.display());
    }
    let emit = |l: Line, lines: &mut Vec<Line>| {
        if !json_out {
            println!("{} {} — {}", l.mark, l.name, l.detail);
        }
        lines.push(l);
    };

    let fs = fs_checks(&work);
    // серверу нужно удалять файлы канала; где удалять нельзя — работает только клиент
    let can_delete = !fs.iter().any(|l| l.name == "удаление файлов" && l.mark != '✓');
    for l in fs {
        emit(l, &mut lines);
    }

    // сервер и клиент в одном процессе, на этой файловой системе
    let ch = work.join("канал .efc");
    let mut cfg = ServerConfig::new(&ch, "efc-selftest", env!("CARGO_PKG_VERSION"));
    cfg.how_to_start = "efc selftest".into();
    cfg.beat = 0.5;
    match Server::start(cfg, Arc::new(Bare)) {
        Err(e) => emit(bad("сервер в процессе", e.to_string()), &mut lines),
        Ok(server) => {
            let modes: &[(bool, &str)] = if can_delete {
                &[(true, "проверочный набор (клиент удаляет)"), (false, "проверочный набор (клиент без права удалять)")]
            } else {
                emit(
                    info(
                        "сервер в этой папке",
                        "не проверяется: удаление запрещено, а серверу оно нужно. Здесь место клиента — сервер запускайте на машине, где папка своя (обычно на хосте)",
                    ),
                    &mut lines,
                );
                &[]
            };
            for &(allow_delete, title) in modes {
                let mut c = Client::open(&ch);
                c.allow_delete = allow_delete;
                c.from = "efc-selftest".into();
                let t = Instant::now();
                let results = check::run_client(&c);
                let failed: Vec<String> = results.iter().filter(|r| !r.ok).map(|r| format!("{}: {}", r.name, r.detail)).collect();
                let l = if failed.is_empty() {
                    ok(title, format!("{} из {} за {:.1} с", results.len(), results.len(), t.elapsed().as_secs_f64()))
                } else {
                    bad(title, failed.join("; "))
                };
                emit(l, &mut lines);
            }
            let c = Client::open(&ch);
            let st = bench(&c, 20);
            let rep = bench_report(&st);
            let med = pct(&st.rtt, 0.5);
            let l = if st.failed > 0 {
                bad("скорость (20 ping)", rep.join("; "))
            } else if med > 300.0 {
                warn("скорость (20 ping)", format!("медиана {med:.0} мс — медленно для локального диска"))
            } else {
                ok("скорость (20 ping)", format!("медиана {med:.0} мс, 95% {:.0} мс", pct(&st.rtt, 0.95)))
            };
            emit(l, &mut lines);
            server.stop(Duration::from_secs(3));
        }
    }

    let cleaned = std::fs::remove_dir_all(&work);
    let failed = lines.iter().filter(|l| l.mark == '✗').count();
    let warned = lines.iter().filter(|l| l.mark == '!').count();
    if json_out {
        let v: Vec<Value> = lines.iter().map(|l| json!({ "mark": l.mark.to_string(), "name": l.name, "detail": l.detail })).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "efc": env!("CARGO_PKG_VERSION"), "failed": failed, "warnings": warned, "checks": v }))
                .unwrap_or_default()
        );
    } else {
        if let Err(e) = cleaned {
            println!("i папку {} убрать не удалось ({}) — удалите вручную", work.display(), err_text(&e));
        }
        println!("{}", if failed == 0 { format!("Итог: всё работает{}", if warned > 0 { format!(", замечаний {warned}") } else { String::new() }) } else { format!("Итог: проблем {failed}, замечаний {warned}") });
    }
    if failed == 0 {
        0
    } else {
        exit::FAILED
    }
}

fn fs_checks(work: &Path) -> Vec<Line> {
    let mut out = Vec::new();
    let a = work.join("a.json");
    let b = work.join("b.json");

    // 1. атомарная запись
    match fsx::write_atomic(&a, b"{\"x\":1}").and_then(|_| std::fs::read(&a)) {
        Ok(v) if v == b"{\"x\":1}" => out.push(ok("атомарная запись", "временный файл + rename")),
        Ok(_) => out.push(bad("атомарная запись", "прочитано не то, что записано")),
        Err(e) => out.push(bad("атомарная запись", err_text(&e))),
    }
    // 2. rename поверх существующего
    let _ = std::fs::write(&b, b"old");
    match fsx::rename_retry(&a, &b) {
        Ok(()) => out.push(ok("rename поверх существующего файла", "ok")),
        Err(e) => out.push(bad("rename поверх существующего файла", err_text(&e))),
    }
    // 3. rename поверх файла, открытого на чтение обычным образом
    {
        let src = work.join("c.json");
        let _ = std::fs::write(&src, b"new");
        let held = std::fs::File::open(&b);
        let t = Instant::now();
        let r = fsx::rename_retry(&src, &b);
        drop(held);
        match r {
            Ok(()) => out.push(ok("rename поверх файла, который кто-то читает", format!("{:.0} мс", t.elapsed().as_secs_f64() * 1000.0))),
            Err(e) => out.push(bad("rename поверх файла, который кто-то читает", err_text(&e))),
        }
    }
    out.extend(windows_lock_checks(work));
    // 4. кириллица и пробелы
    {
        let d = work.join("папка с пробелами и кириллицей");
        let p = d.join("запрос.json");
        match std::fs::create_dir_all(&d).and_then(|_| fsx::write_atomic(&p, b"{}")).and_then(|_| fsx::rename_retry(&p, &d.join("итог.json"))) {
            Ok(()) => out.push(ok("кириллица и пробелы в путях", "ok")),
            Err(e) => out.push(bad("кириллица и пробелы в путях", err_text(&e))),
        }
    }
    // 5. длинный путь
    {
        let mut d = work.to_path_buf();
        for i in 0..6 {
            d = d.join(format!("{i}-{}", "длинное-имя-папки-".repeat(3)));
        }
        let p = d.join("20260101-000000-000-abcdef.json");
        let len = p.as_os_str().len();
        match std::fs::create_dir_all(&d).and_then(|_| fsx::write_atomic(&p, b"{}")).and_then(|_| std::fs::read(&p)) {
            Ok(_) => out.push(ok("длинный путь", format!("{len} байт пути — ok"))),
            Err(e) if cfg!(windows) => out.push(warn(
                "длинный путь",
                format!("{len} байт: {} — каналы с глубокой вложенностью не заработают; включите LongPathsEnabled или держите канал ближе к корню", err_text(&e)),
            )),
            Err(e) => out.push(bad("длинный путь", err_text(&e))),
        }
    }
    // 6. регистр имён
    {
        let p = work.join("case.json");
        let _ = std::fs::write(&p, b"{}");
        let insensitive = work.join("CASE.json").exists();
        out.push(info("регистр имён", if insensitive { "не различается — id канала только строчные, это учтено" } else { "различается" }));
    }
    // 7. точность времени изменения и часы
    {
        let p1 = work.join("t1.json");
        let p2 = work.join("t2.json");
        let _ = std::fs::write(&p1, b"1");
        std::thread::sleep(Duration::from_millis(15));
        let _ = std::fs::write(&p2, b"2");
        let m1 = std::fs::metadata(&p1).and_then(|m| m.modified());
        let m2 = std::fs::metadata(&p2).and_then(|m| m.modified());
        match (m1, m2) {
            (Ok(t1), Ok(t2)) => {
                let diff = t2.duration_since(t1).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
                let sub = t1.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) != 0;
                if diff > 0.0 && sub {
                    out.push(ok("точность времени изменения файлов", format!("файлы с разницей 15 мс различаются ({diff:.0} мс)")));
                } else {
                    out.push(warn("точность времени изменения файлов", "грубая (FAT/exFAT?) — просрочка запросов считается с точностью до секунд"));
                }
                let skew = efc_core::time::age_secs(t2);
                if skew.abs() > 2.0 {
                    out.push(warn("часы файловой системы", format!("время изменения файла отличается от часов системы на {skew:.1} с (сетевой диск?) — просрочка будет неточной")));
                } else {
                    out.push(ok("часы файловой системы", format!("расхождение {:.0} мс", skew * 1000.0)));
                }
            }
            _ => out.push(warn("точность времени изменения файлов", "время изменения не читается")),
        }
    }
    // 8. удаление
    {
        let p = work.join("del.json");
        let _ = std::fs::write(&p, b"{}");
        match std::fs::remove_file(&p) {
            Ok(()) => out.push(ok("удаление файлов", "разрешено — клиент будет удалять сам")),
            Err(e) => out.push(info("удаление файлов", format!("запрещено ({}) — клиент будет переносить в корзину, это штатно", err_text(&e)))),
        }
    }
    out
}

#[cfg(windows)]
fn windows_lock_checks(work: &Path) -> Vec<Line> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    let mut out = Vec::new();
    let name = "rename поверх файла, открытого без разрешения на замену (антивирус, редактор)";
    // держатель открывает файл так, как это делают многие программы: чтение разрешено, замена и удаление — нет
    let target = work.join("locked.json");
    let src = work.join("incoming.json");
    let _ = std::fs::write(&target, b"old");
    let _ = std::fs::write(&src, b"new");
    match std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&target) {
        Err(e) => out.push(warn(name, format!("не удалось открыть файл для пробы: {}", err_text(&e)))),
        Ok(holder) => {
            // сначала — одна попытка без повторов: какой код ошибки даёт Windows
            let first = std::fs::rename(&src, &target);
            let code = first.as_ref().err().map(err_text);
            // затем отпустить файл через 120 мс и проверить, что повтор rename его дожидается
            let h = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                drop(holder);
            });
            let t = Instant::now();
            let r = fsx::rename_retry(&src, &target);
            let _ = h.join();
            match (code, r) {
                (None, _) => out.push(info(name, "Windows разрешила замену сразу — повтор не понадобился")),
                (Some(c), Ok(())) => out.push(ok(name, format!("первая попытка: {c}; повтор дождался файла за {:.0} мс", t.elapsed().as_secs_f64() * 1000.0))),
                (Some(c), Err(e)) => out.push(bad(name, format!("первая попытка: {c}; повторы не помогли: {}", err_text(&e)))),
            }
        }
    }
    // удаление файла, который держат так же: сервер должен убрать его позже, а не падать
    let name = "удаление файла, который держит другая программа";
    let p = work.join("held-del.json");
    let _ = std::fs::write(&p, b"{}");
    if let Ok(holder) = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&p) {
        match std::fs::remove_file(&p) {
            Ok(()) => out.push(info(name, "удалился сразу")),
            Err(e) => out.push(info(name, format!("не удалился ({}) — сервер повторит на следующем такте", err_text(&e)))),
        }
        drop(holder);
        let _ = std::fs::remove_file(&p);
    }
    out
}

#[cfg(not(windows))]
fn windows_lock_checks(_work: &Path) -> Vec<Line> {
    vec![info("блокировки файлов Windows", "проверяются только на Windows")]
}

// ====================================================================== trace

/// Показать журнал трассировки канала: события сервера и клиентов вперемешку, по времени.
pub fn show_trace(layout: &Layout, last: usize, id: Option<&str>, raw: bool) -> i32 {
    let dir = layout.root.join("trace");
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false)).collect(),
        Err(_) => Vec::new(),
    };
    if files.is_empty() {
        eprintln!("Журнала нет: {} пуста. Включите трассировку — EFC_TRACE=1 (или efc --trace) у сервера и клиента.", dir.display());
        return exit::FAILED;
    }
    files.sort();
    let mut events: Vec<(String, Value)> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if let Some(want) = id {
                if v.get("id").and_then(Value::as_str) != Some(want) {
                    continue;
                }
            }
            let t = v.get("t").and_then(Value::as_str).unwrap_or("").to_string();
            events.push((t, v));
        }
    }
    events.sort_by(|a, b| a.0.cmp(&b.0));
    let skip = events.len().saturating_sub(last);
    for (_, v) in events.into_iter().skip(skip) {
        if raw {
            println!("{v}");
            continue;
        }
        let t = v.get("t").and_then(Value::as_str).unwrap_or("");
        let short = t.get(11..23).unwrap_or(t);
        let side = v.get("side").and_then(Value::as_str).unwrap_or("?");
        let ev = v.get("ev").and_then(Value::as_str).unwrap_or("?");
        let mut rest: Vec<String> = Vec::new();
        if let Value::Object(m) = &v {
            for (k, val) in m {
                if ["t", "ms", "side", "ev", "th"].contains(&k.as_str()) || val.is_null() {
                    continue;
                }
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                rest.push(format!("{k}={s}"));
            }
        }
        println!("{short} {side:<6} {ev:<18} {}", rest.join(" "));
    }
    0
}
