//! Сквозные проверки бинарника: `efc serve` в отдельном процессе, клиент — другим процессом.
//! Команды сервера — сам `efc __helper …` (вместо echo/sleep/sh), поэтому тесты идут и на Windows.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const EFC: &str = env!("CARGO_BIN_EXE_efc");

fn temp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("efc-cli-{}-{}", std::process::id(), efc_core::fsx::new_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(channel: &Path, extra: &[&str]) -> Serve {
    let mut cmd = Command::new(EFC);
    cmd.arg("--channel").arg(channel).arg("serve").args(extra).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = cmd.spawn().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !channel.join("alive.json").exists() {
        assert!(Instant::now() < until, "serve не поднялся");
        std::thread::sleep(Duration::from_millis(50));
    }
    Serve(child)
}

/// Команда сервера, которая запускает `efc __helper <args…>`.
fn h(name: &str, args: &[&str], extra: serde_json::Value) -> serde_json::Value {
    let mut a = vec!["__helper".to_string()];
    a.extend(args.iter().map(|s| s.to_string()));
    let mut v = serde_json::json!({ "name": name, "program": EFC, "args": a });
    if let serde_json::Value::Object(m) = extra {
        v.as_object_mut().unwrap().extend(m);
    }
    v
}

/// Записать конфигурацию serve рядом с каналом.
fn config(base: &Path, commands: Vec<serde_json::Value>) -> String {
    let p = base.join(format!("serve-{}.json", efc_core::fsx::new_id()));
    std::fs::write(&p, serde_json::to_vec_pretty(&serde_json::json!({ "commands": commands })).unwrap()).unwrap();
    p.to_string_lossy().into_owned()
}

fn none() -> serde_json::Value {
    serde_json::json!({})
}

fn efc(channel: &Path, args: &[&str]) -> Output {
    Command::new(EFC).arg("--channel").arg(channel).args(args).output().unwrap()
}

fn text(o: &Output) -> (String, String) {
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn serve_call_check_diag() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(&base, vec![h("say", &["echo"], none()), h("fail", &["exit", "1"], none())]);
    let _s = serve(&ch, &["--config", &cfg, "--cwd", base.to_str().unwrap(), "--index", base.to_str().unwrap(), "--title", "Проба"]);

    let o = efc(&ch, &["call", "say", "привет", "--json-не-флаг"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert_eq!(out, "привет --json-не-флаг\n", "всё после имени команды — её аргументы");

    let o = efc(&ch, &["call", "fail"]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(1));
    assert!(err.contains("завершилась с кодом 1"), "{err}");

    let o = efc(&ch, &["call", "nope"]);
    assert_eq!(o.status.code(), Some(3));

    let o = efc(&ch, &["--json", "call", "say", "x"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["exitCode"], 0);
    assert_eq!(v["data"]["exitCode"], 0);

    let o = efc(&ch, &["help"]);
    let (out, _) = text(&o);
    assert!(out.contains("say") && out.contains("trash.clean"), "{out}");

    let o = efc(&ch, &["check"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");

    let o = efc(&ch, &["diag"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("пульс: идёт"), "{out}");

    // поиск по указателю: из корня, по имени
    let o = Command::new(EFC).current_dir(&base).args(["list"]).output().unwrap();
    let (out, _) = text(&o);
    assert!(out.contains("Проба"), "{out}");
    let o = Command::new(EFC).current_dir(&base).args(["--channel", "efc", "call", "say", "по имени"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "по имени\n");
}

#[test]
fn no_wait_wait_and_result() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(&base, vec![h("nap", &["sleep"], none())]);
    let _s = serve(&ch, &["--config", &cfg]);
    let o = efc(&ch, &["--no-wait", "call", "nap", "1"]);
    assert_eq!(o.status.code(), Some(0));
    let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
    assert!(efc_core::fsx::valid_id(&id), "{id}");
    let o = efc(&ch, &["--max", "0.2", "wait", &id]);
    assert_eq!(o.status.code(), Some(5), "короткое ожидание — код 5, вызов идёт");
    let o = efc(&ch, &["--max", "10", "wait", &id]);
    assert_eq!(o.status.code(), Some(0));
    let o = efc(&ch, &["result", &id]);
    assert_eq!(o.status.code(), Some(2), "итог уже забран");
}

#[test]
fn confirm_without_terminal_is_denied() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(&base, vec![h("danger", &["exit", "0"], none())]);
    let _s = serve(&ch, &["--config", &cfg, "--confirm", "danger"]);
    let o = efc(&ch, &["call", "danger"]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(8), "{err}");
}

#[test]
fn fixed_command_from_config_and_timeout() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(
        &base,
        vec![
            h("hello", &["echo", "hello"], serde_json::json!({ "fixed": true, "summary": "Поздороваться" })),
            h("slow", &["sleep", "5"], serde_json::json!({ "fixed": true, "timeout": 0.5 })),
        ],
    );
    let _s = serve(&ch, &["--config", &cfg]);
    let o = efc(&ch, &["call", "hello"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "hello\n");
    let o = efc(&ch, &["call", "hello", "extra"]);
    assert_eq!(o.status.code(), Some(2));
    let o = efc(&ch, &["call", "slow"]);
    assert_eq!(o.status.code(), Some(5));
}

#[test]
fn cancel_through_cli() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(&base, vec![h("nap", &["sleep"], none())]);
    let _s = serve(&ch, &["--config", &cfg]);
    let o = efc(&ch, &["--no-wait", "call", "nap", "30"]);
    let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let o = efc(&ch, &["cancel", &id]);
    assert_eq!(o.status.code(), Some(0));
    let o = efc(&ch, &["--max", "10", "wait", &id]);
    assert_eq!(o.status.code(), Some(7));
}

#[test]
fn no_server_and_dead_server() {
    let base = temp();
    let ch = base.join(".efc");
    std::fs::create_dir_all(&ch).unwrap();
    let o = efc(&ch, &["call", "x"]);
    assert_eq!(o.status.code(), Some(10));

    let cfg = config(&base, vec![h("say", &["echo"], none())]);
    let mut s = serve(&ch, &["--config", &cfg]);
    let _ = s.0.kill(); // гибель процесса: пульс замолкает, паспорт остаётся
    let _ = s.0.wait();
    std::thread::sleep(Duration::from_millis(300));
    // клиенту кажется, что сервер свежий; через порог тишины он отзовёт запрос сам
    let o = efc(&ch, &["call", "say", "x"]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(13), "{err}");
    assert!(err.contains("serve --config"), "подсказка, как запустить: {err}");
    // перезапуск — тот же instance, канал подхватывается
    let _s2 = serve(&ch, &["--config", &cfg]);
    assert_eq!(efc(&ch, &["call", "say", "снова"]).status.code(), Some(0));
}

#[test]
fn init_creates_skeleton() {
    let base = temp();
    let ch = base.join("new-channel");
    let o = Command::new(EFC).args(["init", ch.to_str().unwrap(), "--server", "demo", "--how-to-start", "запустите demo"]).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(ch.join("channel.json")).unwrap()).unwrap();
    assert_eq!(v["protocol"], "efc/1");
    assert_eq!(v["howToStart"], "запустите demo");
    for d in ["requests", "active", "done", "cancel", "trash"] {
        assert!(ch.join(d).is_dir(), "{d}");
    }
}

#[test]
fn timeout_and_cancel_kill_grandchildren() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(
        &base,
        vec![
            h("deep", &["deep", "30"], serde_json::json!({ "fixed": true, "timeout": 1 })),
            h("bg", &["bg", "30"], serde_json::json!({ "fixed": true })),
            h("long", &["deep", "30"], serde_json::json!({ "fixed": true })),
        ],
    );
    let _s = serve(&ch, &["--config", &cfg]);

    let t = Instant::now();
    let o = efc(&ch, &["call", "deep"]);
    assert_eq!(o.status.code(), Some(5));
    assert!(t.elapsed() < Duration::from_secs(8), "предел времени ждал внука: {:?}", t.elapsed());

    let t = Instant::now();
    let o = efc(&ch, &["call", "bg"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("started"));
    assert!(t.elapsed() < Duration::from_secs(8), "итог ждал фоновый процесс: {:?}", t.elapsed());

    let o = efc(&ch, &["--no-wait", "call", "long"]);
    let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    efc(&ch, &["cancel", &id]);
    let o = efc(&ch, &["--max", "20", "wait", &id]);
    assert_eq!(o.status.code(), Some(7));
    assert!(t.elapsed() < Duration::from_secs(8), "отмена ждала внука: {:?}", t.elapsed());
}

#[test]
fn request_timeout_limits_execution() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(&base, vec![h("nap", &["sleep"], none())]);
    let _s = serve(&ch, &["--config", &cfg]);
    let o = efc(&ch, &["--timeout", "1", "--no-wait", "call", "nap", "30"]);
    let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let t = Instant::now();
    let o = efc(&ch, &["--max", "20", "wait", &id]);
    assert_eq!(o.status.code(), Some(5), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(t.elapsed() < Duration::from_secs(8));
}

#[test]
fn no_wait_does_not_turn_wait_timeout_into_success() {
    let base = temp();
    let ch = base.join(".efc");
    let cfg = config(&base, vec![h("nap", &["sleep"], none())]);
    let _s = serve(&ch, &["--config", &cfg]);
    let o = efc(&ch, &["--no-wait", "call", "nap", "5"]);
    let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let o = efc(&ch, &["--no-wait", "--max", "0.2", "wait", &id]);
    assert_eq!(o.status.code(), Some(5));
}

#[test]
fn trace_records_both_sides() {
    let base = temp();
    let ch = base.join(".efc");
    let mut child = Command::new(EFC)
        .args(["--trace", "--channel"])
        .arg(&ch)
        .args(["serve", "--config", &config(&base, vec![h("say", &["echo"], none())])])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !ch.join("alive.json").exists() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(50));
    }
    let o = Command::new(EFC).args(["--trace", "--json", "--channel"]).arg(&ch).args(["call", "say", "x"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let o = efc(&ch, &["trace", "--id", &id, "--raw"]);
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&o.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let evs: Vec<(String, String)> = lines.iter().map(|l| (l["side"].as_str().unwrap().to_string(), l["ev"].as_str().unwrap().to_string())).collect();
    for want in [("client", "call.submit"), ("server", "req.claim"), ("server", "call.accept"), ("server", "call.finish"), ("client", "call.result")] {
        assert!(evs.iter().any(|(s, e)| s == want.0 && e == want.1), "нет {want:?} в {evs:?}");
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn selftest_passes_here() {
    let base = temp();
    let o = Command::new(EFC).args(["--json", "selftest"]).arg(&base).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&o.stdout)));
    assert_eq!(v["failed"], 0, "{v}");
    assert_eq!(o.status.code(), Some(0));
    assert!(v["checks"].as_array().unwrap().len() >= 10);
    assert!(std::fs::read_dir(&base).unwrap().next().is_none(), "selftest убрал за собой");
}

#[test]
fn bench_reports_breakdown() {
    let base = temp();
    let ch = base.join(".efc");
    let _s = serve(&ch, &["--config", &config(&base, vec![h("say", &["echo"], none())])]);
    let o = efc(&ch, &["bench", "-n", "5"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("запрос ждал, пока сервер заберёт") && out.contains("вызовов: 5, без итога: 0"), "{out}");
}

#[test]
fn allow_list_from_flag() {
    let base = temp();
    let ch = base.join(".efc");
    let allow = format!("me={EFC}");
    let _s = serve(&ch, &["--allow", &allow]);
    let o = efc(&ch, &["call", "me", "__helper", "echo", "через", "--allow"]);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "через --allow\n");
    let o = efc(&ch, &["call", "me", "__helper", "exit", "4"]);
    assert_eq!(o.status.code(), Some(1), "программа с кодом 4 → вызов с кодом 1, её код — в data");
    let o = efc(&ch, &["--json", "call", "me", "__helper", "exit", "4"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["data"]["exitCode"], 4);
}

#[test]
fn batch_file_with_steps() {
    let base = temp();
    let ch = base.join(".efc");
    let _s = serve(&ch, &["--config", &config(&base, vec![h("say", &["echo"], none()), h("fail", &["exit", "2"], none())])]);
    let file = base.join("steps.json");
    std::fs::write(
        &file,
        r#"{ "batch": [
            { "id": "a", "command": "say", "args": ["первый"] },
            { "id": "b", "command": "say", "args": ["${a.exitCode}"], "expect": { "output": "0" } },
            { "id": "c", "command": "fail", "expect": { "exit": 1 } },
            { "id": "d", "command": "fail" },
            { "id": "e", "command": "say", "args": ["пропущен"] },
            { "id": "f", "command": "say", "args": ["уборка"], "always": true } ] }"#,
    )
    .unwrap();
    let o = efc(&ch, &["batch", file.to_str().unwrap()]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(out.contains("▸ a say — ok") && out.contains("первый") && out.contains("▸ f say — ok") && out.contains("уборка"), "{out}");
    assert!(!out.contains("пропущен\n"), "шаг e не должен выполняться: {out}");
    assert!(err.contains("✗ d (fail)") && err.contains("пропущены: e"), "{err}");
}
