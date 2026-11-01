//! Сервер файлового канала (SPEC, разделы 3–8, 16): подготовка канала, пульс, приём запросов, пакеты, отмена,
//! ожидание человека, итоги с повтором записи, восстановление после перезапуска, уборка и корзина.
//!
//! Работу делает [`Handler`]; сервер берёт на себя весь протокол. Встроенные команды — `help`, `ping`,
//! `trash.clean`: их обработчик переопределить не может.

use crate::fsx::{self, list_json, stem, write_json};
use crate::model::*;
use crate::time;
use crate::trace;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// Настройки сервера.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Папка канала.
    pub root: PathBuf,
    /// Кто владеет каналом (SPEC 4). Обычно `<сервер>@<машина>:<путь>`.
    pub instance: String,
    /// Имя программы-сервера.
    pub server: String,
    pub version: String,
    /// Имя канала в указателе `.efc-channels.json`.
    pub name: String,
    pub title: String,
    /// Что сделать человеку, чтобы сервер заработал.
    pub how_to_start: String,
    /// Корни путей относительно папки канала; `channel` добавляется сам.
    pub roots: BTreeMap<String, String>,
    /// Период пульса, с.
    pub beat: f64,
    pub limits: Limits,
    /// Дополнительные возможности для `features` (сервер сам добавляет `cancel`, `batch`).
    pub features: Vec<String>,
    pub reasons: Vec<String>,
    pub state: BTreeMap<String, String>,
    pub subchannels: BTreeMap<String, String>,
    /// Папка, в которой вести указатель `.efc-channels.json` (обычно корень примонтированной папки).
    pub index_dir: Option<PathBuf>,
    /// Файлы клиента, которые положить в канал (с контрольной суммой в паспорте).
    pub clients: Vec<PathBuf>,
    /// Обслуживать канал с чужим `instance` (перехватить папку).
    pub take_over: bool,
    /// Период опроса папок.
    pub poll: Duration,
}

impl ServerConfig {
    pub fn new(root: impl Into<PathBuf>, server: &str, version: &str) -> Self {
        let root = root.into();
        // папку создаём сразу: instance строится из канонического пути, и он должен быть тем же при перезапуске
        let _ = std::fs::create_dir_all(&root);
        let host = hostname();
        let abs = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        ServerConfig {
            instance: format!("{server}@{host}:{}", abs.display()),
            server: server.into(),
            version: version.into(),
            name: server.into(),
            title: String::new(),
            how_to_start: String::new(),
            roots: BTreeMap::new(),
            beat: 2.0,
            limits: Limits::default(),
            features: Vec::new(),
            reasons: Vec::new(),
            state: BTreeMap::new(),
            subchannels: BTreeMap::new(),
            index_dir: None,
            clients: Vec::new(),
            take_over: false,
            poll: Duration::from_millis(20),
            root,
        }
    }
}

/// Имя машины: сначала из ОС (стабильно между запусками из разных оболочек), потом из окружения.
pub fn hostname() -> String {
    let from_file = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| from_file("/proc/sys/kernel/hostname"))
        .or_else(|| from_file("/etc/hostname"))
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "host".into())
}

/// Итог работы команды.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub exit: i32,
    pub output: String,
    pub error: Option<String>,
    pub reason: Option<String>,
    pub data: Value,
    pub artifacts: Vec<PathRef>,
}

impl Outcome {
    pub fn ok(output: impl Into<String>) -> Self {
        Outcome { output: output.into(), ..Default::default() }
    }
    pub fn fail(exit: i32, error: impl Into<String>, reason: Option<&str>) -> Self {
        Outcome { exit, error: Some(error.into()), reason: reason.map(String::from), ..Default::default() }
    }
    pub fn cancelled() -> Self {
        Outcome::fail(exit::CANCELLED, "Отменено", Some(reason::CANCELLED))
    }
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = output.into();
        self
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
    pub fn with_artifact(mut self, a: PathRef) -> Self {
        self.artifacts.push(a);
        self
    }
}

/// Контекст вызова: отмена, фазы, ожидание человека, вывод по ходу работы.
pub struct Ctx {
    pub id: String,
    pub from: Option<String>,
    pub force: bool,
    layout: Layout,
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    deadline: Instant,
    /// Сколько запрос пролежал в requests/ до того, как сервер его забрал, мс (по часам сервера).
    pub age_at_claim_ms: f64,
    active: Mutex<Active>,
}

impl Ctx {
    /// Клиент попросил отменить.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Когда вызову пора закончиться (срок запроса). После него сервер просит отменить, а через 30 с
    /// закрывает вызов кодом 5 сам — поздний итог отбрасывается.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Сколько осталось до срока.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Итог уже записан (например, сервер закрыл вызов по сроку) — дальнейшая работа никому не нужна.
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Флаг отмены — для передачи в рабочие потоки.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Сменить фазу (и прогресс 0..1).
    pub fn set_phase(&self, phase: &str, progress: Option<f64>) {
        let mut a = self.active.lock().unwrap();
        a.phase = Some(phase.to_string());
        a.progress = progress.map(|p| p.clamp(0.0, 1.0));
        trace::event("call.phase", json!({ "id": self.id, "phase": phase, "progress": a.progress }));
        self.write(&mut a);
    }

    /// Вызов ждёт человека: `state: waiting` + `asks`.
    pub fn ask_human(&self, text: &str) {
        let mut a = self.active.lock().unwrap();
        a.state = "waiting".into();
        a.asks = Some(Asks { who: "human".into(), text: text.into() });
        trace::event("call.waiting", json!({ "id": self.id, "asks": text }));
        self.write(&mut a);
    }

    /// Снова `running`.
    pub fn resume(&self) {
        let mut a = self.active.lock().unwrap();
        a.state = "running".into();
        a.asks = None;
        self.write(&mut a);
    }

    /// Дописать вывод в `active/<id>.log`.
    pub fn log(&self, text: &str) {
        (self.log_sink())(text)
    }

    /// Дописыватель в `active/<id>.log` — для рабочих потоков.
    pub fn log_sink(&self) -> impl Fn(&str) + Send + 'static {
        let path = self.layout.active_log(&self.id);
        let finished = self.finished.clone();
        move |text: &str| {
            if finished.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f.write_all(text.as_bytes());
            }
        }
    }

    fn write(&self, a: &mut Active) {
        a.updated = Some(time::now_iso());
        // итог уже записан — active/ не возрождать
        if self.finished() {
            return;
        }
        let _ = write_json(&self.layout.active_file(&self.id), a);
        // итог мог появиться между проверкой и записью — убрать возрождённый файл
        if self.finished() {
            let _ = std::fs::remove_file(self.layout.active_file(&self.id));
        }
    }
}

/// Тот, кто выполняет команды.
pub trait Handler: Send + Sync + 'static {
    /// Команды сервера (кроме встроенных).
    fn commands(&self) -> Vec<CommandInfo>;
    /// Выполнить одну команду. Вызывается в отдельном потоке.
    fn run(&self, ctx: &Ctx, op: &Op) -> Outcome;
    /// Состояние подопечного для `alive.json` → `detail`.
    fn detail(&self) -> Value {
        Value::Null
    }
}

/// Почему сервер не стартовал.
#[derive(Debug)]
pub enum StartError {
    /// В папке паспорт чужого `instance`.
    ForeignInstance(String),
    Io(std::io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::ForeignInstance(i) => write!(f, "канал принадлежит другому серверу ({i}); перехватить — take_over"),
            StartError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StartError {}

impl From<std::io::Error> for StartError {
    fn from(e: std::io::Error) -> Self {
        StartError::Io(e)
    }
}

const BUILTIN: [&str; 3] = ["help", "ping", "trash.clean"];
const SWEEP_EVERY: Duration = Duration::from_secs(10);
const TRASH_EVERY: Duration = Duration::from_secs(24 * 3600);
const ORPHAN_AGE: f64 = 3600.0;
const TMP_AGE: f64 = 600.0;
const CANCEL_ORPHAN_AGE: f64 = 60.0;
const PARSE_GRACE: f64 = 2.0;
/// Сколько ждать вызов после его срока (уже с просьбой отменить), прежде чем закрыть кодом 5.
const OVERDUE_GRACE: Duration = Duration::from_secs(30);

/// Выполняемый вызов.
struct CallState {
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    deadline: Instant,
    timeout: f64,
    keep: f64,
}

struct Retry {
    /// Что записать (`None` — только убрать файлы).
    write: Option<(PathBuf, Vec<u8>)>,
    remove: Vec<PathBuf>,
}

struct Inner {
    cfg: ServerConfig,
    layout: Layout,
    handler: Arc<dyn Handler>,
    commands: Vec<CommandInfo>,
    stop: AtomicBool,
    seq: AtomicU64,
    running: Mutex<HashMap<String, Arc<CallState>>>,
    seen: Mutex<HashMap<String, SystemTime>>,
    retry: Mutex<Vec<Retry>>,
    /// Паспорт, который этот сервер записал.
    passport: Mutex<Option<Channel>>,
    /// В channel.json оказался чужой instance — канал не обслуживается.
    foreign: Mutex<Option<String>>,
    /// Запросы, о которых в журнале уже есть «жду дописывания» (чтобы не писать это каждый такт).
    traced: Mutex<HashSet<String>>,
    /// Когда был прошлый пульс (для «пульс опоздал» в журнале).
    last_beat: Mutex<Option<Instant>>,
}

/// Запущенный сервер. `stop()` — штатная остановка; при уничтожении без `stop()` поток тоже останавливается.
pub struct Server {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Подготовить канал и запустить цикл в фоне.
    pub fn start(cfg: ServerConfig, handler: Arc<dyn Handler>) -> Result<Server, StartError> {
        let mut commands = vec![
            CommandInfo::new("help", "Команды сервера", "help [команда]").read_only(),
            CommandInfo::new("ping", "Проверка пути «запрос → итог»", "ping").read_only().idempotent(),
            CommandInfo::new("trash.clean", "Очистить корзину канала", "trash.clean [--dry]"),
        ];
        for c in handler.commands() {
            if !BUILTIN.contains(&c.name.as_str()) {
                commands.push(c);
            }
        }
        let inner = Arc::new(Inner {
            layout: Layout::new(cfg.root.clone()),
            cfg,
            handler,
            commands,
            stop: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            running: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            retry: Mutex::new(Vec::new()),
            passport: Mutex::new(None),
            foreign: Mutex::new(None),
            traced: Mutex::new(HashSet::new()),
            last_beat: Mutex::new(None),
        });
        let trace_to = trace::init_from_env(&inner.layout.root, "server");
        trace::event(
            "server.start",
            json!({
                "server": inner.cfg.server, "version": inner.cfg.version, "instance": inner.cfg.instance,
                "root": inner.layout.root, "beat": inner.cfg.beat, "pollMs": inner.cfg.poll.as_millis() as u64,
                "host": hostname(), "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "efc": env!("CARGO_PKG_VERSION"),
                "trace": trace_to,
            }),
        );
        if let Err(e) = inner.prepare() {
            trace::event("server.start_failed", json!({ "error": e.to_string() }));
            return Err(e);
        }
        let worker = inner.clone();
        let thread = std::thread::Builder::new().name("efc-server".into()).spawn(move || worker.run_loop())?;
        Ok(Server { inner, thread: Some(thread) })
    }

    pub fn layout(&self) -> &Layout {
        &self.inner.layout
    }

    /// Сколько вызовов выполняется сейчас.
    pub fn running(&self) -> usize {
        self.inner.running.lock().unwrap().len()
    }

    /// Штатная остановка: `stopping`, попросить текущие вызовы отмениться и дождаться их (не дольше `grace`),
    /// убрать `alive.json`.
    pub fn stop(mut self, grace: Duration) {
        self.shutdown(grace);
    }

    fn shutdown(&mut self, grace: Duration) {
        if self.inner.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        trace::event("server.stop", json!({ "running": self.running(), "graceMs": grace.as_millis() as u64 }));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.inner.beat();
        // текущим вызовам — просьба отменить; не успеют за grace — итог допишут потоки, если процесс ещё жив
        for st in self.inner.running.lock().unwrap().values() {
            st.cancel.store(true, Ordering::SeqCst);
        }
        let until = Instant::now() + grace;
        while self.running() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.inner.flush_retries();
        let _ = std::fs::remove_file(self.inner.layout.alive_json());
    }

    /// Остановить без штатной процедуры (пульс просто замолкает) — для проверки клиентов.
    pub fn kill(mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.shutdown(Duration::from_secs(5));
        }
    }
}

impl Inner {
    // ------------------------------------------------------------------ подготовка

    fn prepare(&self) -> Result<(), StartError> {
        let l = &self.layout;
        std::fs::create_dir_all(&l.root)?;
        if let Some(old) = fsx::read_json::<Channel>(&l.channel_json()) {
            if old.instance != self.cfg.instance && !self.cfg.take_over {
                return Err(StartError::ForeignInstance(old.instance));
            }
        }
        for d in l.dirs() {
            std::fs::create_dir_all(d)?;
        }
        let ignore = l.root.join(".gitignore");
        if !ignore.exists() {
            fsx::write_atomic(&ignore, b"*\n")?;
        }
        let clients = self.install_clients();
        self.recover();
        fsx::write_atomic(&l.readme(), self.readme().as_bytes())?;
        let passport = self.passport(clients);
        write_json(&l.channel_json(), &passport)?;
        *self.passport.lock().unwrap() = Some(passport);
        {
            // выполненные id: итоги в done/ и забранные клиентом (trash/done-<id>.json) — повтор id не выполняется
            let until = SystemTime::now() + Duration::from_secs(ORPHAN_AGE as u64);
            let mut seen = self.seen.lock().unwrap();
            for p in list_json(&l.done()) {
                seen.insert(stem(&p), until);
            }
            for p in list_json(&l.trash()) {
                if let Some(id) = stem(&p).strip_prefix("done-") {
                    seen.insert(id.to_string(), until);
                }
            }
        }
        self.update_index();
        self.clean_trash(false, Some(self.cfg.limits.trash_max_age));
        self.beat();
        Ok(())
    }

    fn passport(&self, clients: Vec<ClientFile>) -> Channel {
        let mut roots = self.cfg.roots.clone();
        roots.entry("channel".into()).or_insert_with(|| ".".into());
        let mut features = vec!["cancel".to_string(), "batch".to_string()];
        for f in &self.cfg.features {
            if !features.contains(f) {
                features.push(f.clone());
            }
        }
        if self.layout.inbox().exists() && !features.iter().any(|f| f == "inbox") {
            features.push("inbox".into());
        }
        Channel {
            protocol: PROTOCOL.into(),
            instance: self.cfg.instance.clone(),
            server: self.cfg.server.clone(),
            version: self.cfg.version.clone(),
            pid: Some(std::process::id()),
            started: Some(time::now_iso()),
            title: self.cfg.title.clone(),
            how_to_start: self.cfg.how_to_start.clone(),
            roots,
            beat: self.cfg.beat,
            limits: self.cfg.limits.clone(),
            features,
            commands: self.commands.clone(),
            reasons: self.cfg.reasons.clone(),
            subchannels: self.cfg.subchannels.clone(),
            state: self.cfg.state.clone(),
            clients,
        }
    }

    fn readme(&self) -> String {
        let mut s = String::new();
        let title = if self.cfg.title.is_empty() { &self.cfg.server } else { &self.cfg.title };
        s.push_str(&format!("Файловый канал efc/1 — {title}.\n\n"));
        s.push_str("Папку ведёт сервер. Клиент кладёт запросы в requests/ и просьбы отменить в cancel/,\n");
        s.push_str("прочитанное переносит в trash/. Остальное пишет сервер — не правьте.\n\n");
        s.push_str("  channel.json   паспорт: команды, лимиты, корни путей\n");
        s.push_str("  alive.json     пульс сервера\n");
        s.push_str("  requests/      запросы → active/ (принят) → done/ (итог)\n");
        s.push_str("  trash/         корзина: чистит сервер (trash.clean, раз в сутки)\n\n");
        if !self.cfg.how_to_start.is_empty() {
            s.push_str(&format!("Если сервер не отвечает: {}\n\n", self.cfg.how_to_start));
        }
        s.push_str("Команды:\n");
        for c in &self.commands {
            s.push_str(&format!("  {:<14} {}\n", c.name, c.summary));
        }
        s.push_str("\nКлиент: efc --channel <эта папка> call <команда> [аргументы]\n");
        s.replace('\n', if cfg!(windows) { "\r\n" } else { "\n" })
    }

    fn install_clients(&self) -> Vec<ClientFile> {
        let mut out = Vec::new();
        for src in &self.cfg.clients {
            let Some(name) = src.file_name() else { continue };
            let dst = self.layout.root.join(name);
            let Ok(bytes) = std::fs::read(src) else { continue };
            if std::fs::read(&dst).map(|b| b != bytes).unwrap_or(true) && fsx::write_atomic(&dst, &bytes).is_err() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755));
            }
            if let Ok(sha) = fsx::sha256_file(&dst) {
                out.push(ClientFile { file: name.to_string_lossy().into_owned(), sha256: sha });
            }
        }
        out
    }

    /// Вызовы прошлого экземпляра в active/ — их итог неизвестен.
    fn recover(&self) {
        for p in list_json(&self.layout.active()) {
            let id = stem(&p);
            if !fsx::valid_id(&id) || self.layout.done_file(&id).exists() {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            let a: Option<Active> = fsx::read_json(&p);
            trace::event("server.recover", json!({ "id": id }));
            let mut d = Done {
                id: id.clone(),
                exit_code: exit::CRASHED,
                error: Some("Сервер перезапущен во время вызова — результат неизвестен".into()),
                reason: Some(reason::SERVER_RESTARTED.into()),
                accepted: a.and_then(|a| a.accepted),
                ..Default::default()
            };
            self.finish(&id, &mut d, self.cfg.limits.default_keep, None);
        }
    }

    fn update_index(&self) {
        let Some(dir) = &self.cfg.index_dir else { return };
        let file = dir.join(INDEX_FILE);
        let root = std::fs::canonicalize(&self.layout.root).unwrap_or_else(|_| self.layout.root.clone());
        let base = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        let rel = root.strip_prefix(&base).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| root.to_string_lossy().into_owned());
        let mut list: Vec<IndexEntry> = fsx::read_json(&file).unwrap_or_default();
        let entry = IndexEntry { name: self.cfg.name.clone(), path: rel, title: self.cfg.title.clone() };
        if list.contains(&entry) {
            return;
        }
        list.retain(|e| e.name != entry.name && e.path != entry.path);
        list.push(entry);
        list.sort_by(|a, b| a.name.cmp(&b.name));
        let _ = write_json(&file, &list);
    }

    // ------------------------------------------------------------------ цикл

    fn run_loop(self: Arc<Self>) {
        let beat = Duration::from_secs_f64(self.cfg.beat.max(0.1));
        let mut next_beat = Instant::now() + beat;
        let mut next_sweep = Instant::now() + SWEEP_EVERY;
        let mut next_trash = Instant::now() + TRASH_EVERY;
        while !self.stop.load(Ordering::SeqCst) {
            self.flush_retries();
            if self.foreign.lock().unwrap().is_none() {
                self.poll_cancels();
                self.poll_requests();
            }
            self.watch_deadlines();
            let now = Instant::now();
            if now >= next_beat {
                next_beat = now + beat;
                self.beat();
            }
            if now >= next_sweep {
                next_sweep = now + SWEEP_EVERY;
                self.check_passport();
                self.sweep();
            }
            if now >= next_trash {
                next_trash = now + TRASH_EVERY;
                let o = self.clean_trash(false, Some(self.cfg.limits.trash_max_age));
                trace::event("trash.clean", json!({ "by": "daily", "data": o.data }));
            }
            std::thread::sleep(self.cfg.poll);
        }
    }

    fn beat(&self) {
        let running = self.running.lock().unwrap().len() as u64;
        let foreign = self.foreign.lock().unwrap().clone();
        let state = if self.stop.load(Ordering::SeqCst) {
            "stopping"
        } else if foreign.is_some() {
            "degraded"
        } else if running >= self.cfg.limits.queue {
            "busy"
        } else {
            "ready"
        };
        let mut detail = catch_unwind(AssertUnwindSafe(|| self.handler.detail())).unwrap_or(Value::Null);
        if let Some(f) = foreign {
            detail = json!({ "foreignInstance": f, "note": "канал принадлежит другому серверу — запросы не принимаются" });
        }
        let a = Alive {
            v: V,
            seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
            at: Some(time::now_iso()),
            beat: self.cfg.beat,
            server: Some(self.cfg.server.clone()),
            version: Some(self.cfg.version.clone()),
            pid: Some(std::process::id()),
            state: state.into(),
            detail,
        };
        let seq = a.seq;
        if let Err(e) = write_json(&self.layout.alive_json(), &a) {
            trace::event("beat.fail", json!({ "seq": seq, "err": trace::io_err(&e) }));
        }
        let now = Instant::now();
        let mut last = self.last_beat.lock().unwrap();
        if let Some(prev) = *last {
            let gap = now.duration_since(prev).as_secs_f64();
            if gap > self.cfg.beat * 2.0 {
                trace::event("beat.late", json!({ "seq": seq, "gapMs": (gap * 1000.0).round(), "beatMs": self.cfg.beat * 1000.0 }));
            }
        } else {
            trace::event("beat.first", json!({ "seq": seq, "state": state }));
        }
        *last = Some(now);
    }

    // ------------------------------------------------------------------ запросы

    fn known(&self, id: &str) -> bool {
        self.running.lock().unwrap().contains_key(id) || self.seen.lock().unwrap().contains_key(id) || self.layout.done_file(id).exists()
    }

    fn poll_requests(self: &Arc<Self>) {
        for path in list_json(&self.layout.requests()) {
            let id = stem(&path);
            if !fsx::valid_id(&id) {
                continue; // чужое имя — не трогаем
            }
            if self.known(&id) {
                let r = std::fs::remove_file(&path); // повтор id: уже выполнен или выполняется
                trace::event("req.duplicate", json!({ "id": id, "removed": r.is_ok() }));
                continue;
            }
            let age = fsx::age(&path);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let parsed: Result<Request, String> = if size > self.cfg.limits.request_bytes {
                Err(format!("Запрос больше {} байт", self.cfg.limits.request_bytes))
            } else {
                match std::fs::read(&path) {
                    Ok(b) => fsx::parse_json::<Request>(&b).map_err(|e| e.to_string()),
                    Err(_) => continue, // исчез или занят — в следующий раз
                }
            };
            if parsed.is_err() && size <= self.cfg.limits.request_bytes && age < PARSE_GRACE {
                if trace::enabled() && self.traced.lock().unwrap().insert(id.clone()) {
                    trace::event("req.parse_wait", json!({ "id": id, "ageMs": (age * 1000.0).round(), "size": size, "err": parsed.as_ref().err() }));
                }
                continue; // писатель не атомарный — дать дописать
            }
            // очередь полна — подождут в requests/; trash.clean проходит любые блокировки (SPEC 7)
            let bypass = matches!(&parsed, Ok(r) if r.command.as_deref() == Some("trash.clean") && r.batch.is_none());
            if !bypass && self.running.lock().unwrap().len() as u64 >= self.cfg.limits.queue {
                if trace::enabled() && self.traced.lock().unwrap().insert(format!("q:{id}")) {
                    trace::event("req.queue_full", json!({ "id": id, "queue": self.cfg.limits.queue }));
                }
                continue;
            }
            // забрать: rename в active/ (тот же rename, что у клиента при отзыве, — выигрывает один)
            let active = self.layout.active_file(&id);
            let claim_started = Instant::now();
            let claimed = fsx::rename_retry(&path, &active);
            trace::event(
                "req.claim",
                json!({ "id": id, "ok": claimed.is_ok(), "ageMs": (age * 1000.0).round(), "size": size, "tookMs": trace::ms_since(claim_started),
                        "err": claimed.as_ref().err().map(trace::io_err) }),
            );
            if trace::enabled() {
                let mut t = self.traced.lock().unwrap();
                t.remove(&id);
                t.remove(&format!("q:{id}"));
            }
            if claimed.is_err() {
                continue;
            }
            let keep_default = self.cfg.limits.default_keep;
            let req = match parsed {
                Ok(r) => r,
                Err(e) => {
                    let too_large = size > self.cfg.limits.request_bytes;
                    let mut d = Done {
                        id: id.clone(),
                        exit_code: exit::USAGE,
                        error: Some(if too_large { e } else { format!("Неверный запрос: {e}") }),
                        reason: Some(if too_large { reason::TOO_LARGE } else { reason::BAD_REQUEST }.into()),
                        ..Default::default()
                    };
                    self.finish(&id, &mut d, keep_default, None);
                    continue;
                }
            };
            self.accept(id, req, age);
        }
    }

    fn keep_of(&self, req: &Request) -> f64 {
        let l = &self.cfg.limits;
        req.keep.unwrap_or(l.default_keep).clamp(l.min_keep, l.max_keep)
    }

    fn accept(self: &Arc<Self>, id: String, req: Request, age: f64) {
        let keep = self.keep_of(&req);
        let accepted = time::now_iso();
        let fail = |code: i32, err: String, why: &str| {
            trace::event("call.reject", json!({ "id": id, "exit": code, "reason": why, "error": err }));
            let mut d = Done { id: id.clone(), exit_code: code, error: Some(err), reason: Some(why.into()), accepted: Some(accepted.clone()), ..Default::default() };
            self.finish(&id, &mut d, keep, None);
        };
        if req.id != id {
            return fail(exit::USAGE, format!("id в запросе («{}») не совпадает с именем файла", req.id), reason::BAD_REQUEST);
        }
        let timeout = req.timeout.filter(|t| *t > 0.0).unwrap_or(self.cfg.limits.default_timeout);
        if age > timeout + 60.0 {
            return fail(exit::TIMEOUT, "Запрос пролежал дольше своего срока и не выполнялся".into(), reason::EXPIRED);
        }
        let ops: Vec<Op> = match (&req.command, &req.batch) {
            (Some(c), None) if !c.is_empty() => vec![Op { command: c.clone(), args: req.args.clone(), params: req.params.clone() }],
            (None, Some(b)) if !b.is_empty() => b.clone(),
            _ => return fail(exit::USAGE, "Нужно либо command, либо непустой batch".into(), reason::BAD_REQUEST),
        };
        let single = req.batch.is_none();
        if single && !self.has_command(&ops[0].command) {
            return fail(exit::UNKNOWN_COMMAND, format!("Нет команды «{}». Список — help", ops[0].command), reason::UNKNOWN_COMMAND);
        }
        trace::event(
            "call.accept",
            json!({ "id": id, "command": ops.iter().map(|o| o.command.clone()).collect::<Vec<_>>(), "from": req.from, "timeout": timeout, "keep": keep,
                    "ageMs": (age * 1000.0).round() }),
        );
        let state = Arc::new(CallState {
            cancel: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs_f64((timeout - age.max(0.0)).max(1.0)),
            timeout,
            keep,
        });
        self.running.lock().unwrap().insert(id.clone(), state.clone());
        let ctx = Ctx {
            id: id.clone(),
            from: req.from.clone(),
            force: req.force,
            layout: self.layout.clone(),
            cancel: state.cancel.clone(),
            finished: state.finished.clone(),
            deadline: state.deadline,
            age_at_claim_ms: (age * 1000.0).round(),
            active: Mutex::new(Active {
                v: V,
                id: id.clone(),
                state: "running".into(),
                command: Some(if single { ops[0].command.clone() } else { format!("batch ({})", ops.len()) }),
                phase: None,
                progress: None,
                asks: None,
                accepted: Some(accepted.clone()),
                updated: None,
            }),
        };
        {
            let mut a = ctx.active.lock().unwrap();
            ctx.write(&mut a);
        }
        let me = self.clone();
        let accepted_for_call = accepted.clone();
        let stop_on_error = req.on_error.as_deref() != Some("continue");
        let spawned = std::thread::Builder::new().name(format!("efc-call-{id}")).spawn(move || {
            let started = Instant::now();
            let mut d = if single { me.run_single(&ctx, &ops[0]) } else { me.run_batch(&ctx, &ops, stop_on_error) };
            d.accepted = Some(accepted_for_call);
            d.seconds = Some((started.elapsed().as_secs_f64() * 100.0).round() / 100.0);
            me.finish(&ctx.id, &mut d, keep, Some(&state));
        });
        if spawned.is_err() {
            self.running.lock().unwrap().remove(&id);
            fail(exit::CRASHED, "Не удалось запустить поток вызова".into(), "spawn-failed");
        }
    }

    fn has_command(&self, name: &str) -> bool {
        self.commands.iter().any(|c| c.name == name)
    }

    fn execute(&self, ctx: &Ctx, op: &Op) -> Outcome {
        if !self.has_command(&op.command) {
            return Outcome::fail(exit::UNKNOWN_COMMAND, format!("Нет команды «{}»", op.command), Some(reason::UNKNOWN_COMMAND));
        }
        let r = catch_unwind(AssertUnwindSafe(|| match op.command.as_str() {
            "help" => self.help(op),
            "ping" => Outcome::ok("pong\n").with_data(json!({ "server": self.cfg.server, "version": self.cfg.version, "ageAtClaimMs": ctx.age_at_claim_ms })),
            "trash.clean" => {
                let dry = op.args.iter().any(|a| a == "--dry") || op.params.get("dry").and_then(Value::as_bool).unwrap_or(false);
                let o = self.clean_trash(dry, None);
                trace::event("trash.clean", json!({ "by": "command", "data": o.data }));
                o
            }
            _ => self.handler.run(ctx, op),
        }));
        r.unwrap_or_else(|p| {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            Outcome::fail(exit::CRASHED, format!("Сбой команды: {msg}"), Some("crashed"))
        })
    }

    fn run_single(&self, ctx: &Ctx, op: &Op) -> Done {
        let o = self.execute(ctx, op);
        let cancel_ignored = ctx.cancelled() && o.exit != exit::CANCELLED;
        Done {
            id: ctx.id.clone(),
            ok: o.exit == 0,
            exit_code: o.exit,
            error: o.error,
            reason: o.reason,
            output: o.output,
            data: o.data,
            artifacts: o.artifacts,
            cancel_ignored,
            ..Default::default()
        }
    }

    fn run_batch(&self, ctx: &Ctx, ops: &[Op], stop_on_error: bool) -> Done {
        let mut items = Vec::new();
        let mut output = String::new();
        let mut first_fail: Option<(i32, String, Option<String>)> = None;
        let mut artifacts = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            if ctx.cancelled() {
                first_fail.get_or_insert((exit::CANCELLED, "Отменено".into(), Some(reason::CANCELLED.into())));
                break;
            }
            ctx.set_phase(&format!("{} ({}/{})", op.command, i + 1, ops.len()), Some(i as f64 / ops.len() as f64));
            let o = self.execute(ctx, op);
            output.push_str(&o.output);
            artifacts.extend(o.artifacts.iter().cloned());
            if o.exit != 0 && first_fail.is_none() {
                let e = o.error.clone().unwrap_or_else(|| format!("код {}", o.exit));
                first_fail = Some((o.exit, format!("{}: {e}", op.command), o.reason.clone()));
            }
            let failed = o.exit != 0;
            items.push(Item { ok: o.exit == 0, exit_code: o.exit, error: o.error, reason: o.reason, data: o.data });
            if failed && stop_on_error {
                break;
            }
        }
        let (code, error, why) = match first_fail {
            Some((c, e, r)) => (c, Some(e), r),
            None => (0, None, None),
        };
        Done { id: ctx.id.clone(), ok: code == 0, exit_code: code, error, reason: why, output, artifacts, items: Some(items), ..Default::default() }
    }

    fn help(&self, op: &Op) -> Outcome {
        let one = op.args.first().cloned().or_else(|| op.params.get("command").and_then(Value::as_str).map(String::from));
        if let Some(name) = one {
            return match self.commands.iter().find(|c| c.name == name) {
                Some(c) => Outcome::ok(format!("{} — {}\n  {}\n", c.name, c.summary, if c.usage.is_empty() { &c.name } else { &c.usage }))
                    .with_data(serde_json::to_value(c).unwrap_or_default()),
                None => Outcome::fail(exit::UNKNOWN_COMMAND, format!("Нет команды «{name}»"), Some(reason::UNKNOWN_COMMAND)),
            };
        }
        let w = self.commands.iter().map(|c| c.name.chars().count()).max().unwrap_or(4);
        let mut s = String::new();
        for c in &self.commands {
            let pad = w - c.name.chars().count();
            s.push_str(&format!("{}{}  {}\n", c.name, " ".repeat(pad), c.summary));
        }
        Outcome::ok(s).with_data(json!({ "commands": self.commands }))
    }

    // ------------------------------------------------------------------ итог

    /// Записать итог, затем убрать active/. Не записалось — повторять на следующих тактах. С `state` итог
    /// пишется один раз: поздний итог вызова, уже закрытого по сроку, отбрасывается.
    fn finish(&self, id: &str, d: &mut Done, keep: f64, state: Option<&CallState>) {
        if let Some(st) = state {
            if st.finished.swap(true, Ordering::SeqCst) {
                return;
            }
        }
        d.v = V;
        d.id = id.to_string();
        d.state = "done".into();
        d.ok = d.exit_code == 0;
        d.finished = Some(time::now_iso());
        d.expires = Some(time::format(SystemTime::now() + Duration::from_secs_f64(keep)));
        let max = self.cfg.limits.output_chars as usize;
        if d.output.chars().count() > max {
            d.output = d.output.chars().take(max).collect::<String>() + "\n… (вывод обрезан)\n";
            d.truncated = true;
        }
        let path = self.layout.done_file(id);
        let mut bytes = serde_json::to_vec_pretty(d).unwrap_or_default();
        bytes.push(b'\n');
        let remove = vec![self.layout.active_file(id), self.layout.active_log(id)];
        let write_started = Instant::now();
        if fsx::write_atomic(&path, &bytes).is_ok() {
            let left = remove_all(&remove);
            trace::event(
                "call.finish",
                json!({ "id": id, "exit": d.exit_code, "reason": d.reason, "seconds": d.seconds, "writeMs": trace::ms_since(write_started),
                        "notRemoved": left }),
            );
            if !left.is_empty() {
                self.retry.lock().unwrap().push(Retry { write: None, remove: left });
            }
        } else {
            trace::event("call.finish", json!({ "id": id, "exit": d.exit_code, "writeOk": false, "queuedForRetry": true }));
            self.retry.lock().unwrap().push(Retry { write: Some((path, bytes)), remove });
        }
        self.seen.lock().unwrap().insert(id.to_string(), SystemTime::now() + Duration::from_secs_f64(keep.max(ORPHAN_AGE)));
        self.running.lock().unwrap().remove(id);
    }

    /// Срок вызова вышел: попросить отменить; через 30 с — закрыть кодом 5 самому.
    fn watch_deadlines(&self) {
        let now = Instant::now();
        let overdue: Vec<(String, Arc<CallState>)> = self
            .running
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, st)| now >= st.deadline)
            .map(|(id, st)| (id.clone(), st.clone()))
            .collect();
        for (id, st) in overdue {
            if !st.cancel.swap(true, Ordering::SeqCst) {
                trace::event("call.overdue", json!({ "id": id, "action": "cancel", "timeout": st.timeout }));
            }
            if now >= st.deadline + OVERDUE_GRACE {
                trace::event("call.overdue", json!({ "id": id, "action": "close" }));
                let mut d = Done {
                    id: id.clone(),
                    exit_code: exit::TIMEOUT,
                    error: Some(format!("Вызов не уложился в срок ({:.0} с) и не остановился по просьбе — закрыт сервером", st.timeout)),
                    reason: Some("timeout".into()),
                    ..Default::default()
                };
                self.finish(&id, &mut d, st.keep, Some(&st));
            }
        }
    }

    fn flush_retries(&self) {
        let list: Vec<Retry> = std::mem::take(&mut *self.retry.lock().unwrap());
        let total = list.len();
        let mut left = Vec::new();
        for r in list {
            let written = match &r.write {
                Some((path, bytes)) => fsx::write_atomic(path, bytes).is_ok(),
                None => true,
            };
            if written {
                let rest = remove_all(&r.remove);
                if !rest.is_empty() {
                    left.push(Retry { write: None, remove: rest });
                }
            } else {
                left.push(r);
            }
        }
        if total > 0 {
            trace::event("retry", json!({ "pending": total, "left": left.len() }));
        }
        self.retry.lock().unwrap().extend(left);
    }

    // ------------------------------------------------------------------ отмена

    fn poll_cancels(&self) {
        for path in list_json(&self.layout.cancel()) {
            let id = stem(&path);
            if !fsx::valid_id(&id) {
                if fsx::age(&path) > CANCEL_ORPHAN_AGE {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            if let Some(st) = self.running.lock().unwrap().get(&id) {
                st.cancel.store(true, Ordering::SeqCst);
                let _ = std::fs::remove_file(&path);
                trace::event("cancel", json!({ "id": id, "where": "running" }));
                continue;
            }
            let req = self.layout.request(&id);
            if self.known(&id) {
                // уже выполнен: итог не трогать; повтор запроса с этим id — убрать
                trace::event("cancel", json!({ "id": id, "where": "done" }));
                let _ = std::fs::remove_file(&req);
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if req.exists() {
                let req_keep = fsx::read_json::<Request>(&req).map(|r| self.keep_of(&r)).unwrap_or(self.cfg.limits.default_keep);
                if fsx::rename_retry(&req, &self.layout.active_file(&id)).is_ok() {
                    trace::event("cancel", json!({ "id": id, "where": "queued" }));
                    let mut d = Done {
                        id: id.clone(),
                        exit_code: exit::CANCELLED,
                        error: Some("Отменено до начала выполнения".into()),
                        reason: Some(reason::CANCELLED.into()),
                        ..Default::default()
                    };
                    self.finish(&id, &mut d, req_keep, None);
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            if fsx::age(&path) > CANCEL_ORPHAN_AGE {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    // ------------------------------------------------------------------ уборка

    /// Паспорт на месте и наш? Пропал — записать заново; подменён чужим (синхронизация, копия) — перестать
    /// обслуживать канал, пока он чужой.
    fn check_passport(&self) {
        let path = self.layout.channel_json();
        let current: Option<Channel> = fsx::read_json(&path);
        let mut foreign = self.foreign.lock().unwrap();
        match current {
            Some(c) if c.instance != self.cfg.instance => {
                if foreign.is_none() {
                    trace::event("passport", json!({ "foreignInstance": c.instance }));
                    eprintln!("[efc] {}: channel.json подменён ({}) — канал больше не обслуживается", self.layout.root.display(), c.instance);
                }
                *foreign = Some(c.instance);
            }
            Some(_) => *foreign = None,
            None => {
                if path.exists() {
                    return; // занят или дописывается — в следующий раз
                }
                if let Some(p) = self.passport.lock().unwrap().as_ref() {
                    trace::event("passport", json!({ "restored": true }));
                    let _ = write_json(&path, p);
                }
                *foreign = None;
            }
        }
    }

    fn sweep(&self) {
        // журналы трассировки старше 3 суток
        if let Ok(rd) = std::fs::read_dir(self.layout.root.join("trace")) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_file() && fsx::age(&p) > 3.0 * 86_400.0 {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        let now = SystemTime::now();
        self.seen.lock().unwrap().retain(|_, until| *until > now);
        for p in list_json(&self.layout.done()) {
            let d: Option<Done> = fsx::read_json(&p);
            match d.as_ref().and_then(|d| d.expires.as_deref()).and_then(time::parse) {
                Some(exp) if exp < now => {
                    let _ = std::fs::remove_file(&p);
                }
                Some(_) => {}
                None if fsx::age(&p) > ORPHAN_AGE => {
                    let _ = std::fs::remove_file(&p);
                }
                None => {}
            }
        }
        let running: Vec<String> = self.running.lock().unwrap().keys().cloned().collect();
        if let Ok(rd) = std::fs::read_dir(self.layout.active()) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                let id = name.split('.').next().unwrap_or("").to_string();
                if !running.contains(&id) && !name.contains(".tmp") && fsx::age(&p) > ORPHAN_AGE {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        for dir in [self.layout.root.clone(), self.layout.requests(), self.layout.active(), self.layout.done(), self.layout.cancel(), self.layout.trash()] {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if n.contains(".tmp") && p.is_file() && fsx::age(&p) > TMP_AGE {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }

    /// Очистить корзину: только обычные файлы прямо в trash/. `older` — только старше стольких секунд.
    fn clean_trash(&self, dry: bool, older: Option<f64>) -> Outcome {
        let (mut files, mut bytes) = (0u64, 0u64);
        if let Ok(rd) = std::fs::read_dir(self.layout.trash()) {
            for e in rd.flatten() {
                let p = e.path();
                let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
                if !meta.file_type().is_file() {
                    continue;
                }
                if let Some(min) = older {
                    if fsx::age(&p) <= min {
                        continue;
                    }
                }
                if dry || std::fs::remove_file(&p).is_ok() {
                    files += 1;
                    bytes += meta.len();
                }
            }
        }
        let size = human_size(bytes);
        let text = if dry {
            format!("Корзина: {files} {}, {size} — ничего не удалено\n", plural(files))
        } else {
            format!("Корзина: удалено {files} {}, {size}\n", plural(files))
        };
        Outcome::ok(text).with_data(json!({ "files": files, "bytes": bytes, "dry": dry }))
    }
}

/// Убрать файлы; вернуть те, что убрать не удалось (кроме уже отсутствующих).
fn remove_all(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| match std::fs::remove_file(p) {
            Ok(()) => false,
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        })
        .cloned()
        .collect()
}

fn plural(n: u64) -> &'static str {
    let (d, h) = (n % 10, n % 100);
    if d == 1 && h != 11 {
        "файл"
    } else if (2..=4).contains(&d) && !(12..=14).contains(&h) {
        "файла"
    } else {
        "файлов"
    }
}

fn human_size(b: u64) -> String {
    if b < 1024 {
        format!("{b} Б")
    } else if b < 1024 * 1024 {
        format!("{} КБ", (b + 512) / 1024)
    } else {
        format!("{:.1} МБ", b as f64 / 1024.0 / 1024.0)
    }
}

/// Проверить, что путь из запроса лежит под корнем канала (SPEC 9). Возвращает локальный путь.
pub fn resolve_in_roots(layout: &Layout, cfg_roots: &BTreeMap<String, String>, r: &PathRef) -> Option<PathBuf> {
    let rel = Path::new(r.path());
    if !safe_relative(rel) {
        return None;
    }
    let name = r.root().unwrap_or(if cfg_roots.contains_key("project") { "project" } else { "channel" });
    let base = if name == "channel" { Some(".".to_string()) } else { cfg_roots.get(name).cloned() }?;
    Some(layout.root.join(base).join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(plural(1), "файл");
        assert_eq!(plural(3), "файла");
        assert_eq!(plural(11), "файлов");
        assert_eq!(plural(22), "файла");
        assert_eq!(human_size(49152), "48 КБ");
    }
}
