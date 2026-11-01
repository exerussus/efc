//! efc — клиент и инструменты файлового канала efc/1.

mod diagnose;
mod helper;
mod serve;

use efc_core::client::{discover, pick, CallOpts, Client, Waited};
use efc_core::model::*;
use efc_core::{check, fsx, time};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "efc — файловый канал efc/1

Использование: efc [флаги] <команда> [аргументы]

Клиент:
  list                              каналы рядом (вверх от текущей папки, .efc-channels.json, EFC_PATH)
  help [команда]                    команды сервера
  call <команда> [аргументы…]       вызвать и дождаться итога (всё после имени команды — её аргументы)
  batch <файл.json|->               пакет: {\"batch\":[{\"command\":…,\"args\":[…]}…], \"onError\":\"stop\"}
  wait <id>                         ждать итога (не дольше --max)
  result <id>                       итог, если уже есть
  cancel <id> [--reason текст]      попросить отменить
  diag                              пульс, очередь, корзина, подсказки
  trash [--clean] [--dry]           корзина канала

Отладка:
  selftest [папка]                  как ведёт себя файловая система этой машины + сервер и клиент в одном процессе
  bench [-n 20]                     задержки вызова с разбивкой (сервер канала должен работать)
  trace [--last 60] [--id <id>] [--raw]   журнал трассировки канала: сервер и клиенты по времени

Для авторов серверов:
  init <папка> [--server имя] [--title …] [--how-to-start …]   заготовка канала
  check                             проверочный набор против сервера канала
  serve --channel <папка> --allow <имя[=программа]>…           сделать каналом консольные программы
        [--config файл.json] [--confirm имя,…] [--cwd папка] [--title …] [--index папка] [--client файл]…
        [--take-over]

Флаги:
  --channel <папка|имя>   канал (иначе EFC_CHANNEL или поиск)
  --json                  итог JSON-ом
  --timeout <с>           срок запроса для сервера
  --keep <с>              сколько хранить итог, если не забран
  --max <с>               сколько ждать клиенту (по умолчанию — срок запроса)
  --no-wait               вернуть id, как только вызов принят
  --force                 передать серверу force
  --params <json>         объект params для call
  --from <имя>            подпись запроса
  --trace                 трассировка в <канал>/trace/ (то же, что EFC_TRACE=1)
  --trace-to <файл|stderr>  трассировка в файл или stderr

Код выхода — код вызова: 0 успех, 1 ошибка, 2 аргументы, 3 нет команды, 4 сбой, 5 срок, 6 занято,
7 отменено, 8 отказано, 10 нет сервера, 11 канал не однозначен, 13 сервер не отвечает.
Окружение: EFC_CHANNEL, EFC_PATH, EFC_FROM, EFC_NO_DELETE=1 (не пытаться удалять — сразу в корзину),
EFC_TRACE=1|stderr|<файл> (трассировка).";

#[derive(Default)]
struct Global {
    channel: Option<String>,
    json: bool,
    timeout: Option<f64>,
    keep: Option<f64>,
    max: Option<f64>,
    no_wait: bool,
    force: bool,
    params: Option<String>,
    from: Option<String>,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = if argv.first().map(String::as_str) == Some("__helper") { helper::run(&argv[1..]) } else { run(argv) };
    // код вне 0..255 (в том числе отрицательный) — сбой, а не «успех»
    ExitCode::from(if (0..=255).contains(&code) { code as u8 } else { exit::CRASHED as u8 })
}

fn fail(code: i32, msg: impl AsRef<str>) -> i32 {
    eprintln!("✗ {}", msg.as_ref());
    code
}

fn num(v: Option<String>, flag: &str) -> Result<f64, String> {
    v.ok_or(format!("{flag}: нужно число"))?.parse::<f64>().map_err(|_| format!("{flag}: нужно число"))
}

fn run(argv: Vec<String>) -> i32 {
    let mut g = Global::default();
    let mut rest: Vec<String> = Vec::new();
    let mut it = argv.into_iter();
    // общие флаги — до команды и после неё, кроме хвоста call (там всё — аргументы команды)
    while let Some(a) = it.next() {
        let r: Result<(), String> = match a.as_str() {
            "--channel" => it.next().map(|v| g.channel = Some(v)).ok_or("--channel: нужна папка или имя".into()),
            "--json" => {
                let _: () = g.json = true;
                Ok(())
            },
            "--timeout" => num(it.next(), "--timeout").map(|v| g.timeout = Some(v)),
            "--keep" => num(it.next(), "--keep").map(|v| g.keep = Some(v)),
            "--max" => num(it.next(), "--max").map(|v| g.max = Some(v)),
            "--no-wait" => {
                let _: () = g.no_wait = true;
                Ok(())
            },
            "--force" => {
                let _: () = g.force = true;
                Ok(())
            },
            "--params" => it.next().map(|v| g.params = Some(v)).ok_or("--params: нужен JSON".into()),
            "--from" => it.next().map(|v| g.from = Some(v)).ok_or("--from: нужно имя".into()),
            "--trace" if rest.is_empty() || rest[0] != "call" => {
                if efc_core::trace::requested().is_none() {
                    std::env::set_var("EFC_TRACE", "1");
                }
                Ok(())
            }
            "--trace-to" => it.next().map(|v| std::env::set_var("EFC_TRACE", v)).ok_or("--trace-to: нужен файл или stderr".into()),
            "-h" | "--help" if rest.is_empty() => {
                println!("{USAGE}");
                return 0;
            }
            "-V" | "--version" if rest.is_empty() => {
                println!("efc {} ({PROTOCOL})", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            _ => {
                rest.push(a.clone());
                if rest.len() == 2 && rest[0] == "call" {
                    rest.extend(it.by_ref());
                }
                Ok(())
            }
        };
        if let Err(e) = r {
            return fail(exit::USAGE, e);
        }
    }
    let Some(cmd) = rest.first().cloned() else {
        println!("{USAGE}");
        return exit::USAGE;
    };
    let args: Vec<String> = rest[1..].to_vec();
    match cmd.as_str() {
        "list" => cmd_list(&g),
        "help" => with_client(&g, |c| print_waited(&g, c, c.call("help", &args, Value::Null, &opts(&g)))),
        "call" => cmd_call(&g, &args),
        "batch" => cmd_batch(&g, &args),
        "wait" => cmd_wait(&g, &args),
        "result" => cmd_result(&g, &args),
        "cancel" => cmd_cancel(&g, &args),
        "diag" => with_client(&g, |c| cmd_diag(&g, c)),
        "trash" => cmd_trash(&g, &args),
        "init" => cmd_init(&args),
        "check" => cmd_check(&g, &args),
        "serve" => serve::run(&g.channel, &args),
        "selftest" => {
            let base = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            if efc_core::trace::requested().is_some() {
                efc_core::trace::init_from_env(&base, "selftest");
            }
            diagnose::selftest(&base, g.json)
        }
        "bench" => {
            let n = flag(&args, "-n").and_then(|v| v.parse().ok()).unwrap_or(20usize).clamp(1, 1000);
            with_client(&g, |c| {
                if c.alive().is_none() {
                    return print_waited(&g, c, Waited::NoServer);
                }
                println!("Замер: {n} вызовов ping в {}", c.layout.root.display());
                let st = diagnose::bench(c, n);
                let rep = diagnose::bench_report(&st);
                for l in &rep {
                    println!("{l}");
                }
                if st.failed > 0 {
                    exit::FAILED
                } else {
                    0
                }
            })
        }
        "trace" => {
            let last = flag(&args, "--last").and_then(|v| v.parse().ok()).unwrap_or(60usize);
            let id = flag(&args, "--id");
            let raw = args.iter().any(|a| a == "--raw");
            with_client(&g, |c| diagnose::show_trace(&c.layout, last, id.as_deref(), raw))
        }
        "version" => {
            println!("efc {} ({PROTOCOL})", env!("CARGO_PKG_VERSION"));
            0
        }
        other => fail(exit::USAGE, format!("Нет команды клиента «{other}». Вызвать команду сервера — efc call {other} …; справка — efc --help")),
    }
}

fn opts(g: &Global) -> CallOpts {
    CallOpts { timeout: g.timeout, keep: g.keep, no_wait: g.no_wait, max_wait: g.max, force: g.force }
}

fn with_client(g: &Global, f: impl FnOnce(&Client) -> i32) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = match pick(g.channel.as_deref(), &cwd) {
        Ok(r) => r,
        Err((code, msg)) => return fail(code, msg),
    };
    let mut c = Client::open(root);
    if let Some(f) = &g.from {
        c.from = f.clone();
    }
    if let Err(e) = c.compatible() {
        return fail(exit::USAGE, e);
    }
    f(&c)
}

// ---------------------------------------------------------------------- вызовы

fn cmd_call(g: &Global, args: &[String]) -> i32 {
    let Some((command, rest)) = args.split_first() else {
        return fail(exit::USAGE, "call: нужна команда. Список — efc help");
    };
    let params = match &g.params {
        Some(p) => match serde_json::from_str::<Value>(p) {
            Ok(v @ Value::Object(_)) => v,
            _ => return fail(exit::USAGE, "--params: нужен JSON-объект"),
        },
        None => Value::Null,
    };
    with_client(g, |c| print_outcome(g, c, c.call(command, rest, params, &opts(g)), g.no_wait))
}

fn cmd_batch(g: &Global, args: &[String]) -> i32 {
    let Some(src) = args.first() else {
        return fail(exit::USAGE, "batch: нужен файл с пакетом или -");
    };
    let text = if src == "-" {
        let mut s = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).is_err() {
            return fail(exit::USAGE, "batch: не прочитать stdin");
        }
        s
    } else {
        match std::fs::read_to_string(src) {
            Ok(s) => s,
            Err(e) => return fail(exit::USAGE, format!("batch: {src}: {e}")),
        }
    };
    let mut req: Request = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => match serde_json::from_str::<Vec<Op>>(&text) {
            Ok(ops) => Request { batch: Some(ops), ..Default::default() },
            Err(e) => return fail(exit::USAGE, format!("batch: не JSON пакета: {e}")),
        },
    };
    if req.batch.as_ref().map(|b| b.is_empty()).unwrap_or(true) {
        return fail(exit::USAGE, "batch: пустой пакет");
    }
    req.id.clear();
    req.command = None;
    req.timeout = g.timeout.or(req.timeout);
    req.keep = g.keep.or(req.keep);
    req.force |= g.force;
    with_client(g, |c| {
        if c.alive().is_none() {
            return print_waited(g, c, Waited::NoServer);
        }
        let id = match c.submit_request(req) {
            Ok(id) => id,
            Err(e) => return fail(exit::FAILED, format!("не записать запрос: {e}")),
        };
        let max = g.max.or(g.timeout).unwrap_or(600.0);
        print_outcome(g, c, c.wait(&id, max, g.no_wait, true), g.no_wait)
    })
}

fn need_id(args: &[String], what: &str) -> Result<String, i32> {
    match args.first() {
        Some(id) if fsx::valid_id(id) => Ok(id.clone()),
        Some(id) => Err(fail(exit::USAGE, format!("{what}: недопустимый id «{id}»"))),
        None => Err(fail(exit::USAGE, format!("{what}: нужен id"))),
    }
}

fn cmd_wait(g: &Global, args: &[String]) -> i32 {
    let id = match need_id(args, "wait") {
        Ok(i) => i,
        Err(c) => return c,
    };
    with_client(g, |c| {
        if c.result(&id).is_none() && !c.layout.active_file(&id).exists() && !c.layout.request(&id).exists() {
            return fail(exit::USAGE, format!("Вызова {id} нет: ни итога, ни в работе, ни в очереди (итог мог быть уже забран или истечь)"));
        }
        print_waited(g, c, c.wait(&id, g.max.unwrap_or(170.0), false, false))
    })
}

fn cmd_result(g: &Global, args: &[String]) -> i32 {
    let id = match need_id(args, "result") {
        Ok(i) => i,
        Err(c) => return c,
    };
    with_client(g, |c| match c.result(&id) {
        Some(d) => {
            c.take(&id);
            print_waited(g, c, Waited::Done(Box::new(d)))
        }
        None => {
            if let Some(a) = c.active(&id) {
                print_waited(g, c, Waited::Pending { id: id.clone(), active: Some(a) })
            } else if c.layout.request(&id).exists() {
                eprintln!("Вызов {id} ещё в очереди: сервер его не принял.");
                exit::TIMEOUT
            } else {
                fail(exit::USAGE, format!("Итога {id} нет (уже забран, истёк или id неверный)"))
            }
        }
    })
}

fn cmd_cancel(g: &Global, args: &[String]) -> i32 {
    let id = match need_id(args, "cancel") {
        Ok(i) => i,
        Err(c) => return c,
    };
    let why = args.iter().position(|a| a == "--reason").and_then(|i| args.get(i + 1)).cloned();
    with_client(g, |c| match c.cancel(&id, why.as_deref()) {
        Ok(()) => {
            println!("Просьба отменить {id} отправлена. Итог — efc wait {id}");
            0
        }
        Err(e) => fail(exit::FAILED, format!("не записать просьбу: {e}")),
    })
}

// ---------------------------------------------------------------------- вывод итога

/// Напечатать исход. `accepted_ok` — «принят» считается успехом (call/batch с --no-wait).
fn print_waited(g: &Global, c: &Client, w: Waited) -> i32 {
    print_outcome(g, c, w, false)
}

fn print_outcome(g: &Global, c: &Client, w: Waited, accepted_ok: bool) -> i32 {
    let code = w.exit_code();
    match w {
        Waited::Done(d) => {
            if g.json {
                println!("{}", serde_json::to_string_pretty(&*d).unwrap_or_default());
                return d.exit_code;
            }
            if !d.output.is_empty() {
                print!("{}", d.output);
                if !d.output.ends_with('\n') {
                    println!();
                }
            }
            if let Some(items) = &d.items {
                for (i, it) in items.iter().enumerate() {
                    if !it.ok {
                        eprintln!("  #{} — код {}: {}", i + 1, it.exit_code, it.error.clone().unwrap_or_default());
                    }
                }
            }
            if !d.ok {
                let e = d.error.clone().unwrap_or_else(|| format!("код {}", d.exit_code));
                if !d.output.contains(&e) {
                    eprintln!("✗ {e}");
                }
            }
            for p in c.local_paths(&d) {
                println!("→ {}", p.display());
            }
            if d.truncated {
                eprintln!("! вывод обрезан сервером");
            }
            if d.cancel_ignored {
                eprintln!("! команда не умеет отменяться — выполнена до конца");
            }
            d.exit_code
        }
        Waited::Pending { id, active } => {
            let phase = active.as_ref().and_then(|a| a.phase.clone());
            let waiting = active.as_ref().and_then(|a| a.asks.clone());
            if g.json {
                println!("{}", json!({ "id": id, "state": "pending", "phase": phase, "asks": waiting }));
            } else {
                println!("{id}");
                let mut s = format!("Вызов {id} принят и идёт");
                if let Some(p) = phase {
                    s.push_str(&format!(" ({p})"));
                }
                if let Some(a) = waiting {
                    s.push_str(&format!("; ждёт: {}", a.text));
                }
                eprintln!("{s}. Итог — efc wait {id}");
            }
            if accepted_ok {
                0
            } else {
                code
            }
        }
        Waited::Withdrawn { id } => fail(code, format!("Срок вышел: сервер не взял запрос {id}, он отозван и не выполнится.")),
        Waited::ServerSilent { id, withdrawn } => {
            let extra = if withdrawn { format!(" Запрос {id} отозван и не выполнится.") } else { format!(" Запрос {id} ждёт в очереди.") };
            fail(code, format!("Сервер не отвечает: пульс стоит.{extra}\n  {}", c.how_to_start()))
        }
        Waited::SilentAfterSubmit { id } => {
            fail(code, format!("Сервер замолчал, но вызов {id} принят и не потерян. Итог — efc wait {id}\n  {}", c.how_to_start()))
        }
        Waited::NoServer => fail(code, format!("Сервера нет.\n  {}", c.how_to_start())),
    }
}

// ---------------------------------------------------------------------- list, diag, trash

fn pulse_text(root: &Path) -> String {
    let l = Layout::new(root);
    let ch: Option<Channel> = fsx::read_json(&l.channel_json());
    match fsx::read_json::<Alive>(&l.alive_json()) {
        None => "нет сервера".into(),
        Some(a) => {
            let age = fsx::age(&l.alive_json());
            let beat = ch.map(|c| c.beat).unwrap_or(a.beat);
            if age < (5.0 * beat).max(10.0) {
                format!("{} (пульс {:.0} с назад)", a.state, age)
            } else {
                format!("молчит {:.0} с", age)
            }
        }
    }
}

fn cmd_list(g: &Global) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let found = discover(&cwd);
    if g.json {
        let v: Vec<Value> = found
            .iter()
            .map(|f| json!({ "name": f.name, "title": f.title, "path": f.root.display().to_string(), "via": f.via, "pulse": pulse_text(&f.root) }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    if found.is_empty() {
        eprintln!("Каналов не найдено (искал вверх от {} и в EFC_PATH).", cwd.display());
        return exit::NO_SERVER;
    }
    for f in &found {
        let title = if f.title.is_empty() { String::new() } else { format!(" — {}", f.title) };
        println!("{}{}\n  {}  [{}]", f.name, title, f.root.display(), pulse_text(&f.root));
    }
    0
}

fn count(dir: &Path) -> (usize, u64) {
    let mut n = 0;
    let mut b = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    n += 1;
                    b += m.len();
                }
            }
        }
    }
    (n, b)
}

fn cmd_diag(g: &Global, c: &Client) -> i32 {
    let l = &c.layout;
    let mut lines: Vec<String> = Vec::new();
    let mut code = 0;
    lines.push(format!("канал: {}", l.root.display()));
    match &c.channel {
        None => {
            lines.push("паспорта нет — сервер ни разу не открывал этот канал".into());
            code = exit::NO_SERVER;
        }
        Some(ch) => {
            let title = if ch.title.is_empty() { String::new() } else { format!(" — {}", ch.title) };
            lines.push(format!("сервер: {} {}{}, {}, pid {}", ch.server, ch.version, title, ch.protocol, ch.pid.unwrap_or(0)));
            lines.push(format!("instance: {}", ch.instance));
            if let Some(s) = &ch.started {
                lines.push(format!("запущен: {s}"));
            }
            for cf in &ch.clients {
                let p = l.root.join(&cf.file);
                match fsx::sha256_file(&p) {
                    Ok(sha) if sha == cf.sha256 => lines.push(format!("клиент {}: контрольная сумма совпадает", cf.file)),
                    Ok(_) => lines.push(format!("! клиент {}: контрольная сумма НЕ совпадает с паспортом", cf.file)),
                    Err(_) => lines.push(format!("! клиента {} нет в канале", cf.file)),
                }
            }
        }
    }
    // пульс: подождать смену seq
    match c.alive() {
        None => {
            lines.push("пульс: нет alive.json — сервер не запущен".into());
            code = exit::NO_SERVER;
        }
        Some(a) => {
            let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(a.beat * 1.5 + 0.5);
            let mut moved = false;
            while std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if c.alive().map(|b| b.seq != a.seq).unwrap_or(false) {
                    moved = true;
                    break;
                }
            }
            if moved {
                lines.push(format!("пульс: идёт (seq {}, beat {} с), состояние {}", a.seq, a.beat, a.state));
            } else {
                lines.push(format!("пульс: стоит (seq {} не меняется {:.1} с) — сервер не отвечает", a.seq, a.beat * 1.5 + 0.5));
                code = exit::SERVER_SILENT;
            }
            if !a.detail.is_null() {
                lines.push(format!("подопечный: {}", a.detail));
            }
        }
    }
    let (rq, _) = count(&l.requests());
    let (dn, _) = count(&l.done());
    let (cn, _) = count(&l.cancel());
    let (tn, tb) = count(&l.trash());
    lines.push(format!("очередь: {rq} в requests/, {dn} итогов в done/, {cn} в cancel/"));
    for p in fsx::list_json(&l.active()) {
        let id = fsx::stem(&p);
        // итог уже есть (сервер не смог убрать active/ — уберёт позже) — не «в работе»
        if l.done_file(&id).exists() || l.trash().join(format!("done-{id}.json")).exists() {
            continue;
        }
        if let Some(a) = fsx::read_json::<Active>(&p) {
            let mut s = format!("  в работе {} — {}", a.id, a.command.unwrap_or_default());
            if let Some(ph) = a.phase {
                s.push_str(&format!(", {ph}"));
            }
            if let Some(ask) = a.asks {
                s.push_str(&format!(" — ЖДЁТ: {}", ask.text));
            }
            if let Some(u) = a.updated.as_deref().and_then(time::parse) {
                s.push_str(&format!(" (обновлено {:.0} с назад)", time::age_secs(u).max(0.0)));
            }
            lines.push(s);
        }
    }
    lines.push(format!("корзина: {tn} файлов, {} КБ{}", tb.div_ceil(1024), if tn > efc_core::client::TRASH_AUTO_CLEAN { " — пора: efc trash --clean" } else { "" }));
    let (inb, _) = count(&l.inbox());
    if inb > 0 {
        lines.push(format!("inbox: {inb} ждут действия"));
    }
    if let Ok(rd) = std::fs::read_dir(l.state()) {
        let names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        if !names.is_empty() {
            lines.push(format!("state/: {}", names.join(", ")));
        }
    }
    lines.push(format!("удаление: {}", if c.allow_delete { "пробовать удалять, при отказе — в корзину" } else { "запрещено (EFC_NO_DELETE) — сразу в корзину" }));
    if code != 0 {
        lines.push(format!("что сделать: {}", c.how_to_start()));
    }
    if g.json {
        println!("{}", json!({ "code": code, "lines": lines }));
    } else {
        for s in lines {
            println!("{s}");
        }
    }
    code
}

fn cmd_trash(g: &Global, args: &[String]) -> i32 {
    let clean = args.iter().any(|a| a == "--clean");
    let dry = args.iter().any(|a| a == "--dry");
    with_client(g, |c| {
        if clean || dry {
            let a: Vec<String> = if dry { vec!["--dry".into()] } else { vec![] };
            return print_waited(g, c, c.call("trash.clean", &a, Value::Null, &opts(g)));
        }
        let (n, b) = count(&c.layout.trash());
        println!("Корзина: {n} файлов, {} КБ. Очистить — efc trash --clean", b.div_ceil(1024));
        0
    })
}

// ---------------------------------------------------------------------- init, check

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn cmd_init(args: &[String]) -> i32 {
    let Some(dir) = args.first().filter(|a| !a.starts_with("--")) else {
        return fail(exit::USAGE, "init: нужна папка канала");
    };
    let root = PathBuf::from(dir);
    let l = Layout::new(&root);
    if l.channel_json().exists() {
        return fail(exit::FAILED, format!("В {} уже есть channel.json", root.display()));
    }
    let server = flag(args, "--server").unwrap_or_else(|| "my-server".into());
    let mut roots = std::collections::BTreeMap::new();
    roots.insert("channel".to_string(), ".".to_string());
    let ch = Channel {
        protocol: PROTOCOL.into(),
        instance: format!("{server}@<машина>:<путь> — заполнит сервер при старте"),
        server: server.clone(),
        version: "0.0.0".into(),
        pid: None,
        started: None,
        title: flag(args, "--title").unwrap_or_default(),
        how_to_start: flag(args, "--how-to-start").unwrap_or_else(|| format!("Запустите {server}")),
        roots,
        beat: 2.0,
        limits: Limits::default(),
        features: vec!["cancel".into(), "batch".into()],
        commands: vec![
            CommandInfo::new("help", "Команды сервера", "help [команда]").read_only(),
            CommandInfo::new("ping", "Проверка пути «запрос → итог»", "ping").read_only().idempotent(),
            CommandInfo::new("trash.clean", "Очистить корзину канала", "trash.clean [--dry]"),
        ],
        reasons: vec![],
        subchannels: Default::default(),
        state: Default::default(),
        clients: vec![],
    };
    let r = (|| -> std::io::Result<()> {
        for d in l.dirs() {
            std::fs::create_dir_all(d)?;
        }
        fsx::write_atomic(&root.join(".gitignore"), b"*\n")?;
        fsx::write_json(&l.channel_json(), &ch)?;
        fsx::write_atomic(&l.readme(), "Файловый канал efc/1. Папку ведёт сервер; клиент пишет только в requests/ и cancel/.\n".as_bytes())
    })();
    match r {
        Ok(()) => {
            println!("Заготовка канала: {}\nСервер при старте перепишет channel.json (instance, pid, команды).", root.display());
            0
        }
        Err(e) => fail(exit::FAILED, e.to_string()),
    }
}

fn cmd_check(g: &Global, args: &[String]) -> i32 {
    let mut g2 = Global { channel: g.channel.clone(), ..Default::default() };
    if let Some(a) = args.first() {
        g2.channel = Some(a.clone());
    }
    with_client(&g2, |c| {
        println!("Проверка канала {}", c.layout.root.display());
        let results = check::run(&c.layout.root);
        let failed = results.iter().filter(|r| !r.ok).count();
        for r in &results {
            println!("{} {} — {}", if r.ok { "✓" } else { "✗" }, r.name, r.detail);
        }
        println!("{} из {} проверок пройдено", results.len() - failed, results.len());
        if failed == 0 {
            0
        } else {
            exit::FAILED
        }
    })
}
