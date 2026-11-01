//! `efc serve` — сделать файловым каналом консольные программы из белого списка.
//!
//! Программа запускается напрямую (без оболочки) в своей группе процессов, аргументы клиента идут ей как есть —
//! если команда не объявлена `fixed`. Опасные команды объявляются `confirm`: каждый вызов подтверждает человек в
//! терминале сервера; без терминала — отказ (код 8). Отмена и предел времени останавливают программу вместе с её
//! дочерними процессами.
//!
//! Осторожно: программа из белого списка получает любые аргументы клиента, а многие программы умеют через них
//! запускать другие (`git -c alias.x=!…`, `make -C /`, `cargo --config`). Для таких — `fixed` или `confirm`.

use efc_core::model::{exit, reason, CommandInfo, Op};
use efc_core::server::{Ctx, Handler, Outcome, Server, ServerConfig};
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecCommand {
    name: String,
    program: String,
    /// Аргументы, которые всегда идут первыми.
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    summary: String,
    /// Каждый вызов подтверждает человек.
    #[serde(default)]
    confirm: bool,
    /// Аргументы клиента не принимаются.
    #[serde(default)]
    fixed: bool,
    /// Предел выполнения, с.
    #[serde(default)]
    timeout: Option<f64>,
    /// Рабочая папка (по умолчанию — --cwd сервера).
    #[serde(default)]
    cwd: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServeConfigFile {
    #[serde(default)]
    commands: Vec<ExecCommand>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    how_to_start: Option<String>,
}

struct ExecHandler {
    commands: Vec<ExecCommand>,
    cwd: PathBuf,
    prompt: Mutex<()>,
}

impl Handler for ExecHandler {
    fn commands(&self) -> Vec<CommandInfo> {
        self.commands
            .iter()
            .map(|c| {
                let mut summary = if c.summary.is_empty() { format!("{} {}", c.program, c.args.join(" ")).trim().to_string() } else { c.summary.clone() };
                if c.confirm {
                    summary.push_str(" (с подтверждением человека)");
                }
                let usage = if c.fixed { c.name.clone() } else { format!("{} [аргументы]", c.name) };
                CommandInfo { long: true, ..CommandInfo::new(&c.name, &summary, &usage) }
            })
            .collect()
    }

    fn run(&self, ctx: &Ctx, op: &Op) -> Outcome {
        let Some(c) = self.commands.iter().find(|c| c.name == op.command) else {
            return Outcome::fail(exit::UNKNOWN_COMMAND, format!("Нет команды «{}»", op.command), Some(reason::UNKNOWN_COMMAND));
        };
        if c.fixed && !op.args.is_empty() {
            return Outcome::fail(exit::USAGE, format!("Команда «{}» не принимает аргументов", c.name), Some(reason::BAD_REQUEST));
        }
        let line = std::iter::once(&c.program).chain(c.args.iter()).chain(op.args.iter()).map(|a| show_arg(a)).collect::<Vec<_>>().join(" ");
        if c.confirm {
            match self.confirm(ctx, &line) {
                Answer::Yes => {}
                Answer::No => return Outcome::fail(exit::DENIED, "Человек не подтвердил вызов", Some(reason::NO_APPROVAL)),
                Answer::Nobody => return Outcome::fail(exit::DENIED, "Подтвердить некому: сервер запущен без терминала", Some(reason::NO_APPROVAL)),
                Answer::Gone => return Outcome::cancelled(),
            }
        }
        exec(ctx, c, &op.args, c.cwd.as_ref().map(|p| self.cwd.join(p)).unwrap_or_else(|| self.cwd.clone()))
    }
}

enum Answer {
    Yes,
    No,
    Nobody,
    /// Вызов отменён или вышел его срок, пока ждали ответа.
    Gone,
}

/// Строки терминала сервера: один читатель на весь процесс, чтобы брошенный вопрос не съел ответ на следующий.
fn terminal_lines() -> &'static Mutex<Receiver<String>> {
    static LINES: OnceLock<Mutex<Receiver<String>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Mutex::new(rx)
    })
}

impl ExecHandler {
    fn confirm(&self, ctx: &Ctx, line: &str) -> Answer {
        let from: String = ctx.from.clone().unwrap_or_else(|| "клиент".into()).chars().filter(|c| !c.is_control()).take(64).collect();
        ctx.ask_human(&format!("Подтвердите в терминале сервера: {line}"));
        let _g = self.prompt.lock().unwrap();
        if !std::io::stdin().is_terminal() {
            eprintln!("[efc] {line} — подтвердить некому (нет терминала): отказ");
            return Answer::Nobody;
        }
        let rx = terminal_lines().lock().unwrap();
        while rx.try_recv().is_ok() {} // старые ответы не в счёт
        eprint!("[efc] {from} просит выполнить: {line}\n      Разрешить? [y/N]: ");
        let _ = std::io::stderr().flush();
        loop {
            if ctx.cancelled() || ctx.remaining().is_zero() {
                eprintln!("\n[efc] вопрос снят: вызов отменён или вышел его срок");
                return Answer::Gone;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => {
                    return if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes" | "д" | "да") {
                        ctx.resume();
                        Answer::Yes
                    } else {
                        Answer::No
                    };
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Answer::Nobody,
            }
        }
    }
}

/// Аргумент для показа человеку: с пробелами, кавычками или управляющими символами — в кавычках с экранированием,
/// чтобы клиент не мог «перерисовать» строку подтверждения.
fn show_arg(a: &str) -> String {
    if a.is_empty() || a.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '\'' || c == '\\') {
        format!("{a:?}")
    } else {
        a.to_string()
    }
}

/// Остановить программу вместе с её дочерними процессами.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // программа запущена в своей группе процессов (process_group(0)): убить всю группу
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Дождаться читателей вывода, но не дольше `limit`: дочерние процессы программы могут держать трубы открытыми.
fn join_pumps(done: &Receiver<()>, n: usize, limit: Duration) -> bool {
    let until = Instant::now() + limit;
    for _ in 0..n {
        let left = until.saturating_duration_since(Instant::now());
        if done.recv_timeout(left).is_err() {
            return false;
        }
    }
    true
}

fn exec(ctx: &Ctx, c: &ExecCommand, extra: &[String], cwd: PathBuf) -> Outcome {
    ctx.set_phase(&format!("запуск {}", c.program), None);
    let mut cmd = Command::new(&c.program);
    cmd.args(&c.args).args(extra).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(ch) => ch,
        Err(e) => return Outcome::fail(exit::FAILED, format!("Не запустить {}: {e}", c.program), Some("spawn-failed")),
    };
    ctx.set_phase(&format!("{} работает", c.program), None);
    let out = Arc::new(Mutex::new(String::new()));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let mut pumps = 0;
    for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)].into_iter().flatten() {
        let out = out.clone();
        let log = ctx.log_sink();
        let done_tx = done_tx.clone();
        pumps += 1;
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let s = String::from_utf8_lossy(&buf).into_owned();
                        log(&s);
                        out.lock().unwrap().push_str(&s);
                    }
                }
            }
            let _ = done_tx.send(());
        });
    }
    let started = Instant::now();
    // предел — меньшее из предела команды и срока запроса
    let limit = match c.timeout.map(Duration::from_secs_f64) {
        Some(t) => t.min(ctx.remaining()),
        None => ctx.remaining(),
    };
    let finish = |stopped: Option<Outcome>, status: Option<std::process::ExitStatus>| -> Outcome {
        let complete = join_pumps(&done_rx, pumps, Duration::from_secs(2));
        let mut text = out.lock().unwrap().clone();
        if !complete {
            text.push_str("\n! вывод процессов, оставшихся после программы, не дождались\n");
        }
        if let Some(o) = stopped {
            return o.with_output(text);
        }
        match status.and_then(|s| s.code()) {
            Some(0) => Outcome::ok(text).with_data(json!({ "exitCode": 0 })),
            Some(n) => Outcome::fail(exit::FAILED, format!("{} завершилась с кодом {n}", c.program), Some("program-failed"))
                .with_output(text)
                .with_data(json!({ "exitCode": n })),
            None => Outcome::fail(exit::CRASHED, format!("{} прервана сигналом", c.program), Some("program-killed")).with_output(text),
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                // программа вышла; её дочерние процессы, если остались, не держат итог дольше 2 с
                return finish(None, Some(s));
            }
            Ok(None) => {}
            Err(_) => return finish(None, None),
        }
        if ctx.cancelled() {
            kill_tree(&mut child);
            // отмену по сроку запроса прислал сам сервер — это «срок вышел», а не «отменено»
            let o = if ctx.remaining().is_zero() {
                Outcome::fail(exit::TIMEOUT, format!("{} не уложилась в срок запроса — остановлена", c.program), Some("timeout"))
            } else {
                Outcome::cancelled()
            };
            return finish(Some(o), None);
        }
        if started.elapsed() > limit {
            kill_tree(&mut child);
            let o = Outcome::fail(exit::TIMEOUT, format!("{} не уложилась в {:.0} с — остановлена", c.program, limit.as_secs_f64()), Some("timeout"));
            return finish(Some(o), None);
        }
        // короткие программы — частая проверка, долгие — реже
        let el = started.elapsed().as_millis() as u64;
        std::thread::sleep(Duration::from_millis(if el < 200 { 2 } else if el < 2000 { 20 } else { 50 }));
    }
}

fn parse_allow(spec: &str) -> Vec<ExecCommand> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (name, program) = s.split_once('=').unwrap_or((s, s));
            ExecCommand { name: name.trim().into(), program: program.trim().into(), args: vec![], summary: String::new(), confirm: false, fixed: false, timeout: None, cwd: None }
        })
        .collect()
}

pub fn run(channel: &Option<String>, args: &[String]) -> i32 {
    let Some(root) = channel.clone() else {
        eprintln!("✗ serve: нужна --channel <папка>");
        return exit::USAGE;
    };
    let mut commands: Vec<ExecCommand> = Vec::new();
    let mut confirm: Vec<String> = Vec::new();
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut title: Option<String> = None;
    let mut how: Option<String> = None;
    let mut index: Option<PathBuf> = None;
    let mut clients: Vec<PathBuf> = Vec::new();
    let mut take_over = false;
    let mut name: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |flag: &str| it.next().cloned().ok_or_else(|| format!("{flag}: нужно значение"));
        let r: Result<(), String> = match a.as_str() {
            "--allow" => val("--allow").map(|v| commands.extend(parse_allow(&v))),
            "--confirm" => val("--confirm").map(|v| confirm.extend(v.split(',').map(|s| s.trim().to_string()))),
            "--cwd" => val("--cwd").map(|v| cwd = PathBuf::from(v)),
            "--title" => val("--title").map(|v| title = Some(v)),
            "--how-to-start" => val("--how-to-start").map(|v| how = Some(v)),
            "--index" => val("--index").map(|v| index = Some(PathBuf::from(v))),
            "--client" => val("--client").map(|v| clients.push(PathBuf::from(v))),
            "--name" => val("--name").map(|v| name = Some(v)),
            "--take-over" => {
                take_over = true;
                Ok(())
            }
            "--config" => val("--config").and_then(|v| {
                let text = std::fs::read_to_string(&v).map_err(|e| format!("--config {v}: {e}"))?;
                let f: ServeConfigFile = serde_json::from_str(&text).map_err(|e| format!("--config {v}: {e}"))?;
                commands.extend(f.commands);
                if title.is_none() {
                    title = f.title;
                }
                if how.is_none() {
                    how = f.how_to_start;
                }
                Ok(())
            }),
            other => Err(format!("serve: неизвестный флаг «{other}»")),
        };
        if let Err(e) = r {
            eprintln!("✗ {e}");
            return exit::USAGE;
        }
    }
    if commands.is_empty() {
        eprintln!("✗ serve: нужен хотя бы один --allow <имя[=программа]> или --config");
        return exit::USAGE;
    }
    for c in commands.iter_mut() {
        if confirm.iter().any(|n| n == &c.name || n == "*") {
            c.confirm = true;
        }
    }
    let root_path = PathBuf::from(&root);
    let mut cfg = ServerConfig::new(&root_path, "efc-serve", env!("CARGO_PKG_VERSION"));
    cfg.name = name.unwrap_or_else(|| root_path.file_name().map(|n| n.to_string_lossy().trim_start_matches('.').to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "efc-serve".into()));
    cfg.title = title.unwrap_or_default();
    let argv: Vec<String> = std::env::args().collect();
    cfg.how_to_start = how.unwrap_or_else(|| format!("На машине с этой папкой запустите: {}", shell_line(&argv)));
    if let Some(rel) = relative(&root_path, &cwd) {
        cfg.roots.insert("project".into(), rel);
    }
    cfg.index_dir = index;
    cfg.clients = clients;
    cfg.take_over = take_over;
    cfg.reasons = vec!["program-failed".into(), "program-killed".into(), "spawn-failed".into(), "timeout".into(), reason::NO_APPROVAL.into()];
    let handler = Arc::new(ExecHandler { commands, cwd, prompt: Mutex::new(()) });
    let names: Vec<String> = handler.commands.iter().map(|c| c.name.clone()).collect();
    let server = match Server::start(cfg, handler) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ serve: {e}");
            return exit::FAILED;
        }
    };
    eprintln!("[efc] канал {} обслуживается: {}. Остановить — Ctrl+C.", server.layout().root.display(), names.join(", "));
    static STOP: AtomicBool = AtomicBool::new(false);
    if ctrlc::set_handler(|| STOP.store(true, Ordering::SeqCst)).is_err() {
        eprintln!("[efc] ! Ctrl+C не перехвачен — при остановке пульс просто замолчит");
    }
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("[efc] останавливаюсь: текущие вызовы просим отмениться (до 10 с)…");
    server.stop(Duration::from_secs(10));
    eprintln!("[efc] остановлен");
    0
}

fn shell_line(argv: &[String]) -> String {
    argv.iter().map(|a| if a.contains(' ') || a.is_empty() { format!("\"{a}\"") } else { a.clone() }).collect::<Vec<_>>().join(" ")
}

/// Путь `to` относительно `from` (оба приводятся к абсолютным).
fn relative(from: &Path, to: &Path) -> Option<String> {
    let f = std::fs::canonicalize(from).ok().or_else(|| std::env::current_dir().ok().map(|c| c.join(from)))?;
    let t = std::fs::canonicalize(to).ok()?;
    let fc: Vec<_> = f.components().collect();
    let tc: Vec<_> = t.components().collect();
    let common = fc.iter().zip(&tc).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return Some(t.to_string_lossy().into_owned());
    }
    let mut parts: Vec<String> = std::iter::repeat("..".to_string()).take(fc.len() - common).collect();
    parts.extend(tc[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    Some(if parts.is_empty() { ".".into() } else { parts.join("/") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_args_are_escaped() {
        assert_eq!(show_arg("status"), "status");
        assert_eq!(show_arg("a b"), "\"a b\"");
        assert_eq!(show_arg("x\r\x1b[2Kgit status"), "\"x\\r\\u{1b}[2Kgit status\"");
        assert_eq!(show_arg(""), "\"\"");
    }
}
