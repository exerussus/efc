//! Проверочный набор: чёрный ящик против любого сервера efc/1 (SPEC 16). Команды, которые он вызывает, —
//! только стандартные (`ping`, `help`, `trash.clean`) и заведомо несуществующие, так что набор безопасен для
//! живого сервера.

use crate::client::{CallOpts, Client, Waited};
use crate::fsx;
use crate::model::*;
use crate::steps::FEATURE_STEPS;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Результат одной проверки.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

type Check = fn(&Client) -> Result<String, String>;

const CHECKS: &[(&str, Check)] = &[
    ("паспорт channel.json", passport),
    ("пульс alive.json", pulse),
    ("ping: запрос → active → done", ping),
    ("неизвестная команда → 3", unknown),
    ("недописанный/битый JSON → 2", bad_json),
    ("id не совпадает с именем файла → 2", id_mismatch),
    ("просроченный запрос не выполняется → 5", expired),
    ("чужие имена и временные файлы не трогаются", foreign_files),
    ("повтор id не выполняется", duplicate),
    ("отзыв запроса без гонки", withdraw),
    ("пакет: итог по каждому элементу", batch),
    ("шаги пакета: ${…}, expect, continue, always", steps_check),
    ("trash.clean и --dry", trash),
    ("help", help),
];

/// Прогнать все проверки.
pub fn run(channel: &Path) -> Vec<CheckResult> {
    let mut c = Client::open(channel);
    c.from = "efc-check".into();
    run_client(&c)
}

/// Прогнать все проверки готовым клиентом (например, без права удалять).
pub fn run_client(c: &Client) -> Vec<CheckResult> {
    CHECKS
        .iter()
        .map(|(name, f)| match f(c) {
            Ok(detail) => CheckResult { name, ok: true, detail },
            Err(detail) => CheckResult { name, ok: false, detail },
        })
        .collect()
}

fn opts() -> CallOpts {
    CallOpts { timeout: Some(30.0), keep: Some(60.0), max_wait: Some(15.0), ..Default::default() }
}

fn done(w: Waited) -> Result<Done, String> {
    match w {
        Waited::Done(d) => Ok(*d),
        other => Err(format!("нет итога: {other:?}")),
    }
}

/// Дождаться итога по id (файл кладём сами, минуя клиент).
fn wait_done(c: &Client, id: &str, secs: f64) -> Option<Done> {
    let until = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < until {
        if let Some(d) = c.result(id) {
            c.take(id);
            return Some(d);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn raw_request(c: &Client, id: &str, body: &[u8], age: Option<Duration>) -> Result<(), String> {
    let p = c.layout.request(id);
    fsx::write_atomic(&p, body).map_err(|e| e.to_string())?;
    if let Some(a) = age {
        let f = std::fs::File::options().write(true).open(&p).map_err(|e| e.to_string())?;
        f.set_modified(SystemTime::now() - a).map_err(|e| e.to_string())?;
    }
    Ok(())
}


fn expect(d: &Done, code: i32, why: Option<&str>) -> Result<String, String> {
    if d.exit_code != code {
        return Err(format!("ожидался код {code}, получен {} ({})", d.exit_code, d.error.clone().unwrap_or_default()));
    }
    if d.ok != (code == 0) {
        return Err("ok не совпадает с exitCode".into());
    }
    if let Some(w) = why {
        if d.reason.as_deref() != Some(w) {
            return Err(format!("ожидалась reason «{w}», получена {:?}", d.reason));
        }
    }
    Ok(format!("код {}{}", d.exit_code, d.reason.as_ref().map(|r| format!(", {r}")).unwrap_or_default()))
}

// ---------------------------------------------------------------------- проверки

fn passport(c: &Client) -> Result<String, String> {
    let ch = c.channel.as_ref().ok_or("нет channel.json или он не разбирается")?;
    if ch.major() != Some(1) {
        return Err(format!("protocol = «{}», нужно efc/1", ch.protocol));
    }
    if ch.instance.is_empty() {
        return Err("пустой instance".into());
    }
    if !ch.commands.iter().any(|x| x.name == "trash.clean") {
        return Err("нет обязательной команды trash.clean".into());
    }
    for d in c.layout.dirs() {
        if !d.is_dir() {
            return Err(format!("нет папки {}", d.display()));
        }
    }
    let mut notes = Vec::new();
    if ch.how_to_start.is_empty() {
        notes.push("нет howToStart (следует)");
    }
    if !c.layout.root.join(".gitignore").exists() {
        notes.push("нет .gitignore");
    }
    Ok(if notes.is_empty() { format!("{} {}", ch.server, ch.version) } else { notes.join("; ") })
}

fn pulse(c: &Client) -> Result<String, String> {
    let a = c.alive().ok_or("нет alive.json")?;
    let wait = Duration::from_secs_f64(a.beat * 3.0 + 1.0);
    let until = Instant::now() + wait;
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(b) = c.alive() {
            if b.seq > a.seq {
                return Ok(format!("seq {} → {}, beat {} с", a.seq, b.seq, a.beat));
            }
        }
    }
    Err(format!("seq не вырос за {:.0} с", wait.as_secs_f64()))
}

fn ping(c: &Client) -> Result<String, String> {
    let d = done(c.call("ping", &[], Value::Null, &opts()))?;
    expect(&d, 0, None)?;
    if d.v != 1 || d.state != "done" || d.expires.is_none() {
        return Err("в итоге нет v=1, state=done или expires".into());
    }
    // итог пишется раньше, чем убирается active/; в общей папке ВМ удаление к тому же видно с задержкой
    let until = Instant::now() + Duration::from_secs(3);
    while c.layout.active_file(&d.id).exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    if c.layout.active_file(&d.id).exists() {
        return Err("active/ не убран через 3 с после итога".into());
    }
    Ok(format!("{} с", d.seconds.unwrap_or(0.0)))
}

fn unknown(c: &Client) -> Result<String, String> {
    let d = done(c.call("efc-check.no-such-command", &[], Value::Null, &opts()))?;
    expect(&d, exit::UNKNOWN_COMMAND, Some(reason::UNKNOWN_COMMAND))
}

fn bad_json(c: &Client) -> Result<String, String> {
    let id = fsx::new_id();
    raw_request(c, &id, b"{ \"v\": 1, \"id\": ", Some(Duration::from_secs(10)))?;
    let d = wait_done(c, &id, 10.0).ok_or("итога нет")?;
    expect(&d, exit::USAGE, Some(reason::BAD_REQUEST))
}

fn id_mismatch(c: &Client) -> Result<String, String> {
    let id = fsx::new_id();
    let body = serde_json::to_vec(&json!({ "v": 1, "id": "other-id", "command": "ping" })).unwrap();
    raw_request(c, &id, &body, None)?;
    let d = wait_done(c, &id, 10.0).ok_or("итога нет")?;
    expect(&d, exit::USAGE, Some(reason::BAD_REQUEST))
}

fn expired(c: &Client) -> Result<String, String> {
    let id = fsx::new_id();
    let body = serde_json::to_vec(&json!({ "v": 1, "id": id, "command": "ping", "timeout": 1 })).unwrap();
    // сначала — во временное имя: состарить, проверить, и только потом показать серверу
    let tmp_id = format!("{id}-x");
    let old = Duration::from_secs(7200);
    let tmp = c.layout.requests().join(format!(".{tmp_id}.json"));
    fsx::write_atomic(&tmp, &body).map_err(|e| e.to_string())?;
    let f = std::fs::File::options().write(true).open(&tmp).map_err(|e| e.to_string())?;
    let set = f.set_modified(SystemTime::now() - old);
    drop(f);
    let took = fsx::age(&tmp) > old.as_secs_f64() - 60.0;
    if set.is_err() || !took {
        fsx::dispose(&tmp, &c.layout.trash(), &format!("check-{tmp_id}.json"), c.allow_delete);
        return Ok("пропущено: файловая система не даёт сменить время изменения файла".into());
    }
    fsx::rename_retry(&tmp, &c.layout.request(&id)).map_err(|e| e.to_string())?;
    let d = wait_done(c, &id, 10.0).ok_or("итога нет")?;
    expect(&d, exit::TIMEOUT, Some(reason::EXPIRED))
}

fn foreign_files(c: &Client) -> Result<String, String> {
    let bad = c.layout.requests().join("Not An Id.json");
    let tmp = c.layout.requests().join(format!("{}.json.tmp-check", fsx::new_id()));
    std::fs::write(&bad, br#"{"v":1,"id":"x","command":"ping"}"#).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, br#"{"v":1}"#).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(1500));
    let ok = bad.exists() && tmp.exists();
    // убрать за собой (переносом — у клиента может не быть права удалять)
    for p in [&bad, &tmp] {
        let name = format!("check-{}", p.file_name().unwrap().to_string_lossy().replace(' ', "_"));
        fsx::dispose(p, &c.layout.trash(), &name, c.allow_delete);
    }
    if ok {
        Ok("не тронуты".into())
    } else {
        Err("сервер тронул файл с недопустимым именем или временный файл".into())
    }
}

fn duplicate(c: &Client) -> Result<String, String> {
    let id = c.submit("ping", &[], Value::Null, &opts()).map_err(|e| e.to_string())?;
    let first = wait_done(c, &id, 10.0).ok_or("первого итога нет")?;
    expect(&first, 0, None)?;
    let body = serde_json::to_vec(&json!({ "v": 1, "id": id, "command": "ping" })).unwrap();
    raw_request(c, &id, &body, None)?;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if c.layout.done_file(&id).exists() {
            c.take(&id);
            return Err("повтор id выполнен второй раз".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = c.layout.request(&id).exists();
    if left {
        c.withdraw(&id);
        return Ok("не выполнен (но запрос оставлен в requests/ — следует убирать)".into());
    }
    Ok("не выполнен, запрос убран".into())
}

fn withdraw(c: &Client) -> Result<String, String> {
    let id = c.submit("ping", &[], Value::Null, &opts()).map_err(|e| e.to_string())?;
    let withdrawn = c.withdraw(&id);
    std::thread::sleep(Duration::from_millis(1500));
    let d = c.result(&id);
    match (withdrawn, d) {
        (true, None) => Ok("отозван до принятия, не выполнен".into()),
        (false, Some(_)) => {
            c.take(&id);
            Ok("сервер успел принять, итог есть".into())
        }
        (false, None) => match wait_done(c, &id, 10.0) {
            Some(_) => Ok("сервер успел принять, итог есть".into()),
            None => Err("запрос не отозван и итога нет".into()),
        },
        (true, Some(_)) => {
            c.take(&id);
            Err("отозванный запрос всё равно выполнен".into())
        }
    }
}

fn has(c: &Client, f: &str) -> bool {
    c.channel.as_ref().map(|ch| ch.features.iter().any(|x| x == f)).unwrap_or(false)
}

fn op(id: Option<&str>, command: &str) -> Op {
    Op { id: id.map(String::from), command: command.into(), ..Default::default() }
}

fn batch(c: &Client) -> Result<String, String> {
    if !has(c, "batch") {
        return Ok("пропущено: сервер не объявил batch".into());
    }
    let req = Request {
        batch: Some(vec![op(None, "ping"), op(None, "efc-check.no-such-command"), op(None, "ping")]),
        timeout: Some(30.0),
        keep: Some(60.0),
        ..Default::default()
    };
    let id = c.submit_request(req).map_err(|e| e.to_string())?;
    let d = wait_done(c, &id, 15.0).ok_or("итога нет")?;
    let items = d.items.as_ref().ok_or("нет items")?;
    // по элементу на каждую команду: выполненные, затем пропущенные
    let executed: Vec<&Item> = items.iter().filter(|i| !matches!(i.status.as_deref(), Some("skipped") | Some("cancelled"))).collect();
    if executed.len() != 2 || !executed[0].ok || executed[1].exit_code != exit::UNKNOWN_COMMAND || d.exit_code != exit::UNKNOWN_COMMAND {
        return Err(format!("ожидалось: ping ок, затем 3 и остановка; выполнено {}, код {}", executed.len(), d.exit_code));
    }
    if items.len() == 3 && items[2].status.as_deref() != Some("skipped") {
        return Err("третий элемент после остановки должен быть skipped".into());
    }
    Ok("ok, 3, остановка".into())
}

fn steps_check(c: &Client) -> Result<String, String> {
    if !has(c, FEATURE_STEPS) {
        return Ok("пропущено: сервер не объявил steps".into());
    }
    let expect = |e: Value| -> Option<Expect> { Some(serde_json::from_value(e).unwrap()) };
    let ops = vec![
        // a — источник данных
        op(Some("a"), "ping"),
        // b — подстановка в expect: тот же сервер, что у a
        Op { expect: expect(json!({ "data": { "server": "${a.server}" } })), ..op(Some("b"), "ping") },
        // c — ошибка, но пакет продолжается
        Op { on_error: Some("continue".into()), ..op(Some("c"), "efc-check.no-such-command") },
        // d — ожидаемая ошибка: код 3 считается успехом
        Op { expect: expect(json!({ "exit": 3 })), ..op(Some("d"), "efc-check.no-such-command") },
        // e — проверка вывода не проходит → остановка
        Op { expect: expect(json!({ "notOutput": "pong" })), ..op(Some("e"), "ping") },
        // f — пропускается
        op(Some("f"), "ping"),
        // g — уборка: выполняется и после остановки
        Op { always: true, ..op(Some("g"), "ping") },
    ];
    let req = Request { batch: Some(ops), timeout: Some(30.0), keep: Some(60.0), ..Default::default() };
    let id = c.submit_request(req).map_err(|e| e.to_string())?;
    let d = wait_done(c, &id, 15.0).ok_or("итога нет")?;
    let items = d.items.as_ref().ok_or("нет items")?;
    let st: Vec<String> = items.iter().map(|i| format!("{}:{}", i.id.clone().unwrap_or_default(), i.status.clone().unwrap_or_default())).collect();
    let want = ["a:ok", "b:ok", "c:failed", "d:ok", "e:failed", "f:skipped", "g:ok"];
    if st != want {
        return Err(format!("статусы шагов {st:?}, ожидались {want:?}"));
    }
    if items[4].expect_failed.is_empty() || items[4].reason.as_deref() != Some("expect-failed") {
        return Err("у шага e нет expectFailed / reason expect-failed".into());
    }
    if d.ok || d.exit_code != exit::UNKNOWN_COMMAND {
        return Err(format!("итог пакета: ожидался код 3 (первая ошибка — шаг c), получен {}", d.exit_code));
    }
    Ok("подстановка, expect, continue, always — ok".into())
}

fn trash(c: &Client) -> Result<String, String> {
    let name = format!("check-{}.json", fsx::new_id());
    let probe = c.layout.trash().join(&name);
    std::fs::write(&probe, b"{}").map_err(|e| e.to_string())?;
    let dry = done(c.call("trash.clean", &["--dry".into()], Value::Null, &opts()))?;
    expect(&dry, 0, None)?;
    if !probe.exists() {
        return Err("--dry удалил файл".into());
    }
    if dry.data.get("files").and_then(Value::as_u64).unwrap_or(0) < 1 {
        return Err("--dry не насчитал файлов".into());
    }
    let real = done(c.call("trash.clean", &[], Value::Null, &opts()))?;
    expect(&real, 0, None)?;
    // общая папка ВМ может ещё показывать удалённый сервером файл — подождать
    let until = Instant::now() + Duration::from_secs(5);
    let started = Instant::now();
    while probe.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
    if probe.exists() {
        return Err("файл в корзине не удалён (или общая папка не показывает удаление дольше 5 с)".into());
    }
    let lag = started.elapsed().as_secs_f64();
    let note = if lag > 0.2 { format!("; удаление стало видно через {:.1} с", lag) } else { String::new() };
    Ok(format!("{}{note}", real.output.trim()))
}

fn help(c: &Client) -> Result<String, String> {
    let d = done(c.call("help", &[], Value::Null, &opts()))?;
    expect(&d, 0, None)?;
    if !d.output.contains("trash.clean") {
        return Err("в help нет trash.clean".into());
    }
    Ok("ok".into())
}
