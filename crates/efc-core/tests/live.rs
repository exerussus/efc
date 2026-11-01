//! Живой сервер в процессе против клиента: протокол целиком.

use efc_core::client::{discover, CallOpts, Client, Waited};
use efc_core::fsx;
use efc_core::model::*;
use efc_core::server::{Ctx, Handler, Outcome, Server, ServerConfig, StartError};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

struct TestHandler;

impl Handler for TestHandler {
    fn commands(&self) -> Vec<CommandInfo> {
        vec![
            CommandInfo::new("echo", "Повторить аргументы", "echo …"),
            CommandInfo::new("sleep", "Спать N с (отменяемо)", "sleep <с>").long(),
            CommandInfo::new("stubborn", "Спать N с, отмену не слушать", "stubborn <с>").long(),
            CommandInfo::new("ask", "Ждать человека, пока не отменят", "ask").long(),
            CommandInfo::new("spam", "Много вывода", "spam <n>"),
            CommandInfo::new("panic", "Упасть", "panic"),
            CommandInfo::new("artifact", "Вернуть ссылку на файл", "artifact"),
        ]
    }

    fn run(&self, ctx: &Ctx, op: &Op) -> Outcome {
        let secs = || op.args.first().and_then(|a| a.parse::<f64>().ok()).unwrap_or(1.0);
        match op.command.as_str() {
            "echo" => Outcome::ok(op.args.join(" ") + "\n").with_data(json!({ "args": op.args, "params": op.params })),
            "sleep" => {
                let until = Instant::now() + Duration::from_secs_f64(secs());
                let mut i = 0;
                while Instant::now() < until {
                    if ctx.cancelled() {
                        return Outcome::cancelled();
                    }
                    i += 1;
                    ctx.set_phase(&format!("сплю {i}"), None);
                    ctx.log("тик\n");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Outcome::ok("выспался\n")
            }
            "stubborn" => {
                std::thread::sleep(Duration::from_secs_f64(secs()));
                Outcome::ok("всё равно доделал\n")
            }
            "ask" => {
                ctx.ask_human("Нажмите кнопку");
                while !ctx.cancelled() {
                    std::thread::sleep(Duration::from_millis(30));
                }
                Outcome::cancelled()
            }
            "spam" => Outcome::ok("x".repeat(op.args.first().and_then(|a| a.parse().ok()).unwrap_or(100))),
            "panic" => panic!("нарочно"),
            "artifact" => Outcome::ok("").with_artifact(PathRef::rooted("project", "out/report.txt")),
            _ => Outcome::fail(exit::UNKNOWN_COMMAND, "нет", None),
        }
    }

    fn detail(&self) -> Value {
        json!({ "ward": "ok" })
    }
}

fn temp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("efc-test-{}", fsx::new_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(root: &std::path::Path) -> ServerConfig {
    let mut c = ServerConfig::new(root, "test-server", "0.0.1");
    c.beat = 0.2;
    c.how_to_start = "запустите тестовый сервер".into();
    c.roots.insert("project".into(), "..".into());
    c
}

fn start(root: &std::path::Path) -> Server {
    Server::start(config(root), Arc::new(TestHandler)).unwrap()
}

fn opts() -> CallOpts {
    CallOpts { timeout: Some(30.0), keep: Some(60.0), max_wait: Some(10.0), ..Default::default() }
}

fn done(w: Waited) -> Done {
    match w {
        Waited::Done(d) => *d,
        other => panic!("нет итога: {other:?}"),
    }
}

#[test]
fn conformance_suite_passes() {
    let base = temp();
    let root = base.join(".efc");
    let s = start(&root);
    let results = efc_core::check::run(&root);
    for r in &results {
        assert!(r.ok, "{}: {}", r.name, r.detail);
    }
    s.stop(Duration::from_secs(2));
    assert!(!root.join("alive.json").exists(), "штатная остановка убирает пульс");
}

#[test]
fn conformance_suite_passes_without_delete_rights() {
    let base = temp();
    let root = base.join(".efc");
    let s = start(&root);
    let mut c = Client::open(&root);
    c.allow_delete = false;
    let results = efc_core::check::run_client(&c);
    for r in &results {
        assert!(r.ok, "{}: {}", r.name, r.detail);
    }
    drop(s);
}

#[test]
fn call_echo_and_params() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let d = done(c.call("echo", &["привет".into(), "мир".into()], json!({ "k": 1 }), &opts()));
    assert!(d.ok);
    assert_eq!(d.output, "привет мир\n");
    assert_eq!(d.data["params"]["k"], 1);
    assert!(!c.layout.done_file(&d.id).exists(), "клиент забрал итог");
}

#[test]
fn client_without_delete_uses_trash_and_auto_cleans() {
    let root = temp();
    let _s = start(&root);
    let mut c = Client::open(&root);
    c.allow_delete = false;
    let d = done(c.call("ping", &[], Value::Null, &opts()));
    assert!(c.layout.trash().join(format!("done-{}.json", d.id)).exists(), "итог перенесён в корзину");
    for i in 0..60 {
        std::fs::write(c.layout.trash().join(format!("junk-{i}.json")), b"{}").unwrap();
    }
    let _ = done(c.call("ping", &[], Value::Null, &opts())); // корзина > 50 → trash.clean в фоне
    let until = Instant::now() + Duration::from_secs(5);
    while c.trash_count() > 5 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(c.trash_count() <= 5, "корзина очищена сервером: {}", c.trash_count());
}

#[test]
fn no_wait_then_wait() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let w = c.call("sleep", &["0.6".into()], Value::Null, &CallOpts { no_wait: true, ..opts() });
    let id = match w {
        Waited::Pending { id, active } => {
            assert!(active.is_some());
            id
        }
        other => panic!("{other:?}"),
    };
    // короткий срок ожидания — вызов не теряется
    match c.wait(&id, 0.1, false, false) {
        Waited::Pending { .. } => {}
        other => panic!("{other:?}"),
    }
    let d = done(c.wait(&id, 10.0, false, false));
    assert_eq!(d.output, "выспался\n");
    // сервер сначала пишет итог, потом убирает active/ и лог — дать ему закончить
    let until = Instant::now() + Duration::from_secs(2);
    while c.layout.active_log(&id).exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!c.layout.active_log(&id).exists(), "лог вызова убран после итога");
}

#[test]
fn cancel_running_and_stubborn() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let id = match c.call("sleep", &["30".into()], Value::Null, &CallOpts { no_wait: true, ..opts() }) {
        Waited::Pending { id, .. } => id,
        other => panic!("{other:?}"),
    };
    c.cancel(&id, Some("не нужно")).unwrap();
    let d = done(c.wait(&id, 10.0, false, false));
    assert_eq!(d.exit_code, exit::CANCELLED);
    assert_eq!(d.reason.as_deref(), Some(reason::CANCELLED));
    assert!(!c.layout.cancel_file(&id).exists());

    let id = match c.call("stubborn", &["0.5".into()], Value::Null, &CallOpts { no_wait: true, ..opts() }) {
        Waited::Pending { id, .. } => id,
        other => panic!("{other:?}"),
    };
    c.cancel(&id, None).unwrap();
    let d = done(c.wait(&id, 10.0, false, false));
    assert!(d.ok);
    assert!(d.cancel_ignored);
}

#[test]
fn waiting_for_human_is_visible() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let id = match c.call("ask", &[], Value::Null, &CallOpts { no_wait: true, ..opts() }) {
        Waited::Pending { id, .. } => id,
        other => panic!("{other:?}"),
    };
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(a) = c.active(&id) {
            if a.state == "waiting" {
                assert_eq!(a.asks.unwrap().text, "Нажмите кнопку");
                break;
            }
        }
        assert!(Instant::now() < until, "state waiting не появился");
        std::thread::sleep(Duration::from_millis(30));
    }
    c.cancel(&id, None).unwrap();
    assert_eq!(done(c.wait(&id, 10.0, false, false)).exit_code, exit::CANCELLED);
}

#[test]
fn restart_closes_tails() {
    let root = temp();
    let s = start(&root);
    let c = Client::open(&root);
    let id = match c.call("stubborn", &["3".into()], Value::Null, &CallOpts { no_wait: true, ..opts() }) {
        Waited::Pending { id, .. } => id,
        other => panic!("{other:?}"),
    };
    s.kill(); // поток вызова ещё спит; пульс встал
    // имитируем гибель процесса: итог прошлого экземпляра так и не появится
    let new = start(&root);
    let d = done(c.wait(&id, 5.0, false, false));
    assert_eq!(d.exit_code, exit::CRASHED);
    assert_eq!(d.reason.as_deref(), Some(reason::SERVER_RESTARTED));
    drop(new);
}

#[test]
fn foreign_instance_refused() {
    let root = temp();
    let s = start(&root);
    s.stop(Duration::from_secs(1));
    let mut other = config(&root);
    other.instance = "someone-else@elsewhere:/x".into();
    match Server::start(other.clone(), Arc::new(TestHandler)) {
        Err(StartError::ForeignInstance(i)) => assert!(i.starts_with("test-server@")),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("чужой канал взят без take_over"),
    }
    other.take_over = true;
    let s2 = Server::start(other, Arc::new(TestHandler)).expect("take_over");
    drop(s2);
}

#[test]
fn silent_server_withdraws_request() {
    let root = temp();
    let s = start(&root);
    let c = Client::open(&root);
    s.kill();
    std::thread::sleep(Duration::from_millis(1500));
    // пульс стоит: свежесть файла ещё в пределах порога, клиент положит запрос и отзовёт его сам
    let w = c.call("ping", &[], Value::Null, &CallOpts { max_wait: Some(15.0), ..opts() });
    match &w {
        Waited::ServerSilent { id, withdrawn: true } => {
            assert!(!c.layout.request(id).exists(), "запрос отозван");
            assert!(c.layout.trash().join(format!("req-{id}.json")).exists(), "отозванный запрос — в корзине");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(w.exit_code(), exit::SERVER_SILENT);
}

#[test]
fn no_server_at_all() {
    let root = temp();
    let c = Client::open(&root);
    let w = c.call("ping", &[], Value::Null, &opts());
    assert_eq!(w.exit_code(), exit::NO_SERVER);
}

#[test]
fn output_is_truncated_and_panics_are_caught() {
    let root = temp();
    let mut cfg = config(&root);
    cfg.limits.output_chars = 10;
    let _s = Server::start(cfg, Arc::new(TestHandler)).unwrap();
    let c = Client::open(&root);
    let d = done(c.call("spam", &["100".into()], Value::Null, &opts()));
    assert!(d.truncated);
    assert!(d.output.starts_with("xxxxxxxxxx\n"));
    let d = done(c.call("panic", &[], Value::Null, &opts()));
    assert_eq!(d.exit_code, exit::CRASHED);
    assert!(d.error.unwrap().contains("нарочно"));
    // сервер жив после паники команды
    assert!(done(c.call("ping", &[], Value::Null, &opts())).ok);
}

#[test]
fn artifacts_resolve_to_local_paths() {
    let base = temp();
    let root = base.join(".efc");
    let _s = start(&root);
    let c = Client::open(&root);
    let d = done(c.call("artifact", &[], Value::Null, &opts()));
    let p = c.local_paths(&d);
    assert_eq!(p, vec![root.join("..").join("out/report.txt")]);
}

#[test]
fn index_and_discovery() {
    let base = temp();
    let root = base.join("app").join(".efc");
    let mut cfg = config(&root);
    cfg.index_dir = Some(base.clone());
    cfg.name = "app".into();
    cfg.title = "Тестовое приложение".into();
    let _s = Server::start(cfg, Arc::new(TestHandler)).unwrap();
    let idx: Vec<IndexEntry> = fsx::read_json(&base.join(INDEX_FILE)).unwrap();
    assert_eq!(idx, vec![IndexEntry { name: "app".into(), path: "app/.efc".into(), title: "Тестовое приложение".into() }]);
    let found = discover(&base);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "app");
    // и из подпапки проекта — вверх через .efc/
    let found = discover(&base.join("app"));
    assert_eq!(found.len(), 1);
}

#[test]
fn old_trash_is_cleaned_on_start_and_alive_has_detail() {
    let root = temp();
    std::fs::create_dir_all(root.join("trash")).unwrap();
    let old = root.join("trash").join("old.json");
    let fresh = root.join("trash").join("fresh.json");
    std::fs::write(&old, b"{}").unwrap();
    std::fs::write(&fresh, b"{}").unwrap();
    std::fs::File::options().write(true).open(&old).unwrap().set_modified(SystemTime::now() - Duration::from_secs(2 * 86_400)).unwrap();
    let _s = start(&root);
    assert!(!old.exists(), "старый файл корзины удалён при старте");
    assert!(fresh.exists(), "свежий оставлен");
    let a: Alive = fsx::read_json(&root.join("alive.json")).unwrap();
    assert_eq!(a.detail["ward"], "ok");
    let ch: Channel = fsx::read_json(&root.join("channel.json")).unwrap();
    assert!(ch.commands.iter().any(|c| c.name == "sleep"));
    assert_eq!(ch.roots.get("channel").map(String::as_str), Some("."));
}

#[test]
fn many_parallel_calls() {
    let root = temp();
    let _s = start(&root);
    let c = Arc::new(Client::open(&root));
    let threads: Vec<_> = (0..20)
        .map(|i| {
            let c = c.clone();
            std::thread::spawn(move || done(c.call("echo", &[i.to_string()], Value::Null, &opts())).output)
        })
        .collect();
    let mut outs: Vec<String> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    outs.sort();
    assert_eq!(outs.len(), 20);
    assert!(fsx::list_json(&root.join("active")).is_empty());
}

#[test]
fn repeated_id_after_restart_is_not_run() {
    let root = temp();
    let s = start(&root);
    let mut c = Client::open(&root);
    c.allow_delete = false; // итог уходит в trash/done-<id>.json
    let d = done(c.call("echo", &["раз".into()], Value::Null, &opts()));
    s.stop(Duration::from_secs(1));
    let _s = start(&root);
    let body = serde_json::to_vec(&json!({ "v": 1, "id": d.id, "command": "echo", "args": ["два"] })).unwrap();
    fsx::write_atomic(&c.layout.request(&d.id), &body).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(!c.layout.done_file(&d.id).exists(), "повтор id после перезапуска выполнен");
}

#[test]
fn request_deadline_cancels_running_call() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let o = CallOpts { timeout: Some(1.0), no_wait: true, ..opts() };
    let id = match c.call("sleep", &["30".into()], Value::Null, &o) {
        Waited::Pending { id, .. } => id,
        other => panic!("{other:?}"),
    };
    let t = Instant::now();
    let d = done(c.wait(&id, 20.0, false, false));
    assert_eq!(d.exit_code, exit::CANCELLED, "обработчик получил просьбу отмениться по сроку");
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[test]
fn replaced_passport_stops_service() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let mut ch: Channel = fsx::read_json(&root.join("channel.json")).unwrap();
    ch.instance = "other@elsewhere:/x".into();
    fsx::write_json(&root.join("channel.json"), &ch).unwrap();
    // проверка паспорта — на такте уборки (10 с)
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let a: Alive = fsx::read_json(&root.join("alive.json")).unwrap();
        if a.state == "degraded" {
            assert_eq!(a.detail["foreignInstance"], "other@elsewhere:/x");
            break;
        }
        assert!(Instant::now() < until, "сервер не заметил подмену паспорта");
        std::thread::sleep(Duration::from_millis(200));
    }
    let id = c.submit("ping", &[], Value::Null, &opts()).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(c.layout.request(&id).exists(), "чужой канал не обслуживается");
}

#[test]
fn trash_clean_passes_full_queue() {
    let root = temp();
    let mut cfg = config(&root);
    cfg.limits.queue = 1;
    let _s = Server::start(cfg, Arc::new(TestHandler)).unwrap();
    let c = Client::open(&root);
    let busy = c.call("sleep", &["3".into()], Value::Null, &CallOpts { no_wait: true, ..opts() });
    assert!(matches!(busy, Waited::Pending { .. }));
    let d = done(c.call("trash.clean", &[], Value::Null, &CallOpts { max_wait: Some(2.0), ..opts() }));
    assert!(d.ok);
}

fn batch_req(ops: Vec<Op>) -> Request {
    Request { batch: Some(ops), timeout: Some(30.0), keep: Some(60.0), ..Default::default() }
}

fn step(id: &str, command: &str, args: &[&str]) -> Op {
    Op { id: Some(id.into()), command: command.into(), args: args.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

#[test]
fn steps_substitute_args_and_params() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let mut b = step("b", "echo", &["${a.args.0}-${a.args.1}"]);
    b.params = json!({ "from": "${a.args}" });
    let id = c.submit_request(batch_req(vec![step("a", "echo", &["x", "y"]), b])).unwrap();
    let d = done(c.wait(&id, 10.0, false, true));
    assert!(d.ok, "{:?}", d.error);
    let items = d.items.unwrap();
    assert_eq!(items[1].output, "x-y\n");
    assert_eq!(items[1].data["params"]["from"], json!(["x", "y"]));
    assert!(d.output.contains("▸ a echo — ok") && d.output.contains("▸ b echo — ok"), "{}", d.output);
}

#[test]
fn step_bad_reference_fails_without_running() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let id = c.submit_request(batch_req(vec![step("a", "echo", &["${nope.x}"])])).unwrap();
    let d = done(c.wait(&id, 10.0, false, true));
    assert_eq!(d.exit_code, exit::USAGE);
    assert_eq!(d.items.unwrap()[0].reason.as_deref(), Some("bad-reference"));
}

#[test]
fn step_timeout_and_always_after_cancel() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    // шаг со своим пределом: sleep 30 при timeout 0.3 → 5
    let mut slow = step("slow", "sleep", &["30"]);
    slow.timeout = Some(0.3);
    let id = c.submit_request(batch_req(vec![slow])).unwrap();
    let t = Instant::now();
    let d = done(c.wait(&id, 10.0, false, true));
    assert_eq!(d.exit_code, exit::TIMEOUT);
    assert_eq!(d.items.unwrap()[0].reason.as_deref(), Some("step-timeout"));
    assert!(t.elapsed() < Duration::from_secs(3));

    // отмена посреди шага: следующий обычный шаг — cancelled, шаг always — выполняется
    let mut cleanup = step("cleanup", "sleep", &["0.2"]);
    cleanup.always = true;
    let id = c.submit_request(batch_req(vec![step("work", "sleep", &["30"]), step("next", "echo", &["x"]), cleanup])).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    c.cancel(&id, None).unwrap();
    let d = done(c.wait(&id, 10.0, false, true));
    assert_eq!(d.exit_code, exit::CANCELLED);
    let st: Vec<String> = d.items.unwrap().iter().map(|i| i.status.clone().unwrap()).collect();
    assert_eq!(st, vec!["failed", "cancelled", "ok"], "always-шаг выполнен после отмены, сон не прерван отменой вызова");
}

#[test]
fn lock_blocks_changing_calls() {
    let root = temp();
    let _s = start(&root);
    let c = Client::open(&root);
    let mut req = batch_req(vec![step("hold", "sleep", &["1.5"])]);
    req.lock = true;
    let holder = c.submit_request(req).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let d = done(c.call("echo", &["x".into()], Value::Null, &opts()));
    assert_eq!(d.exit_code, exit::BUSY);
    assert_eq!(d.data["busyWith"], holder.as_str());
    // читающие и trash.clean проходят; force — тоже
    assert!(done(c.call("ping", &[], Value::Null, &opts())).ok);
    assert!(done(c.call("trash.clean", &["--dry".into()], Value::Null, &opts())).ok);
    assert!(done(c.call("echo", &["x".into()], Value::Null, &CallOpts { force: true, ..opts() })).ok);
    assert!(done(c.wait(&holder, 10.0, false, true)).ok);
    // блокировка снята с итогом
    assert!(done(c.call("echo", &["x".into()], Value::Null, &opts())).ok);
}

#[test]
fn client_refuses_steps_without_server_support() {
    let root = temp();
    let _s = start(&root);
    let mut c = Client::open(&root);
    c.channel.as_mut().unwrap().features.retain(|f| f != "steps" && f != "lock");
    let mut e = step("a", "ping", &[]);
    e.expect = Some(serde_json::from_value(json!({ "exit": 0 })).unwrap());
    let err = c.submit_request(batch_req(vec![e])).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("steps"));
    let mut req = batch_req(vec![Op { command: "ping".into(), ..Default::default() }]);
    assert!(c.submit_request(req.clone()).is_ok(), "простой пакет без шагов — можно");
    req.lock = true;
    assert!(c.submit_request(req).unwrap_err().to_string().contains("lock"));
}
