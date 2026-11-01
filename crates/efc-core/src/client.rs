//! Клиент файлового канала (SPEC, разделы 3, 5–8, 11, 16): запрос, ожидание итога, отзыв без гонки, отмена,
//! корзина вместо удаления, поиск каналов.

use crate::fsx::{self, Disposed};
use crate::model::*;
use crate::time;
use crate::trace;
use serde_json::json;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Сколько файлов в корзине — повод отправить `trash.clean`.
pub const TRASH_AUTO_CLEAN: usize = 50;
/// Сколько ждать итога принятого вызова, когда пульс встал.
pub const SILENT_AFTER_SUBMIT: f64 = 90.0;

/// Открытый канал.
#[derive(Debug, Clone)]
pub struct Client {
    pub layout: Layout,
    pub channel: Option<Channel>,
    /// Пытаться удалять (`false` — сразу в корзину; задаётся `EFC_NO_DELETE=1`).
    pub allow_delete: bool,
    /// Подпись `from` в запросах.
    pub from: String,
}

/// Чем закончилось ожидание.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Waited {
    /// Итог получен.
    Done(Box<Done>),
    /// Вызов принят, итога пока нет (`--no-wait` или вышел срок ожидания). Итог — позже по id.
    Pending { id: String, active: Option<Active> },
    /// Запрос не принят и отозван: срок вышел.
    Withdrawn { id: String },
    /// Сервер не отвечает, запрос не принят. `withdrawn` — этот клиент его отозвал (свой запрос).
    ServerSilent { id: String, withdrawn: bool },
    /// Принятый вызов, но пульс стоит дольше 90 с.
    SilentAfterSubmit { id: String },
    /// Сервера нет.
    NoServer,
}

impl Waited {
    /// Код процесса для этого исхода (SPEC 13).
    pub fn exit_code(&self) -> i32 {
        match self {
            Waited::Done(d) => d.exit_code,
            Waited::Pending { .. } | Waited::Withdrawn { .. } | Waited::SilentAfterSubmit { .. } => exit::TIMEOUT,
            Waited::ServerSilent { .. } => exit::SERVER_SILENT,
            Waited::NoServer => exit::NO_SERVER,
        }
    }
}

/// Параметры вызова.
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct CallOpts {
    /// Срок запроса для сервера, с.
    pub timeout: Option<f64>,
    /// Сколько серверу хранить итог, с.
    pub keep: Option<f64>,
    /// Вернуться, как только вызов принят.
    pub no_wait: bool,
    /// Сколько ждать самому клиенту, с (по умолчанию — `timeout`).
    pub max_wait: Option<f64>,
    pub force: bool,
}


/// Наблюдатель за пульсом: живость — по смене `seq` и своим часам.
struct Pulse {
    seq: Option<u64>,
    since: Instant,
    beat: f64,
}

impl Pulse {
    fn new(beat: f64) -> Self {
        Pulse { seq: None, since: Instant::now(), beat }
    }
    /// Сколько секунд пульс не менялся.
    fn silent(&mut self, alive: Option<&Alive>) -> f64 {
        let seq = alive.map(|a| a.seq);
        if seq != self.seq {
            if self.seq.is_some() {
                trace::event("pulse", json!({ "seq": seq, "gapMs": (self.since.elapsed().as_secs_f64() * 1000.0).round(), "state": alive.map(|a| a.state.clone()) }));
            }
            self.seq = seq;
            self.since = Instant::now();
        }
        self.since.elapsed().as_secs_f64()
    }
    fn dead_after(&self) -> f64 {
        (5.0 * self.beat).max(10.0)
    }
}

impl Client {
    /// Открыть канал в папке (паспорт может отсутствовать — тогда сервера нет).
    pub fn open(root: impl Into<PathBuf>) -> Client {
        let layout = Layout::new(root.into());
        let channel = fsx::read_json(&layout.channel_json());
        let no_delete = std::env::var("EFC_NO_DELETE").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
        trace::init_from_env(&layout.root, "client");
        Client { layout, channel, allow_delete: !no_delete, from: std::env::var("EFC_FROM").unwrap_or_else(|_| "efc".into()) }
    }

    pub fn alive(&self) -> Option<Alive> {
        fsx::read_json(&self.layout.alive_json())
    }

    fn beat(&self) -> f64 {
        self.alive().map(|a| a.beat).or(self.channel.as_ref().map(|c| c.beat)).unwrap_or(2.0).max(0.1)
    }

    /// Версия протокола понятна этому клиенту.
    pub fn compatible(&self) -> Result<(), String> {
        match &self.channel {
            None => Ok(()),
            Some(c) => match c.major() {
                Some(1) => Ok(()),
                _ => Err(format!("Канал говорит на «{}», клиент понимает только {PROTOCOL}. Обновите клиент.", c.protocol)),
            },
        }
    }

    /// Подсказка человеку, как запустить сервер.
    pub fn how_to_start(&self) -> String {
        match &self.channel {
            Some(c) if !c.how_to_start.is_empty() => c.how_to_start.clone(),
            _ => format!("Запустите сервер, который ведёт канал {}.", self.layout.root.display()),
        }
    }

    /// Положить запрос. Возвращает id.
    pub fn submit(&self, command: &str, args: &[String], params: Value, opts: &CallOpts) -> std::io::Result<String> {
        let req = Request {
            v: V,
            id: fsx::new_id(),
            command: Some(command.to_string()),
            args: args.to_vec(),
            params,
            from: Some(self.from.clone()),
            created: Some(time::now_iso()),
            timeout: opts.timeout,
            keep: opts.keep,
            force: opts.force,
            ..Default::default()
        };
        self.submit_request(req)
    }

    /// Положить готовый запрос (пакет и т. п.); пустой id заменяется новым.
    pub fn submit_request(&self, mut req: Request) -> std::io::Result<String> {
        if req.id.is_empty() {
            req.id = fsx::new_id();
        }
        if !fsx::valid_id(&req.id) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "недопустимый id"));
        }
        req.v = V;
        req.created.get_or_insert_with(time::now_iso);
        req.from.get_or_insert_with(|| self.from.clone());
        let started = Instant::now();
        let r = fsx::write_json(&self.layout.request(&req.id), &req);
        trace::event(
            "call.submit",
            json!({ "id": req.id, "command": req.command, "batch": req.batch.as_ref().map(|b| b.len()), "writeMs": trace::ms_since(started),
                    "err": r.as_ref().err().map(trace::io_err) }),
        );
        r?;
        Ok(req.id)
    }

    /// Вызвать и дождаться (или вернуться при принятии, если `no_wait`).
    pub fn call(&self, command: &str, args: &[String], params: Value, opts: &CallOpts) -> Waited {
        // о живости судим только по смене seq (часы ВМ и хоста расходятся): запрос кладём сразу, а молчание
        // пульса замечает wait — непринятый запрос он отзовёт
        if self.alive().is_none() {
            return Waited::NoServer;
        }
        let id = match self.submit(command, args, params, opts) {
            Ok(id) => id,
            Err(_) => return Waited::NoServer,
        };
        let max = opts.max_wait.or(opts.timeout).unwrap_or_else(|| self.channel.as_ref().map(|c| c.limits.default_timeout).unwrap_or(600.0));
        self.wait(&id, max, opts.no_wait, true)
    }

    /// Ждать итога вызова `id` не дольше `max` секунд. `submitted` — запрос положил этот клиент: если сервер
    /// его не принял, по выходу запрос отзывается.
    pub fn wait(&self, id: &str, max: f64, no_wait: bool, submitted: bool) -> Waited {
        let started = Instant::now();
        let deadline = started + Duration::from_secs_f64(max.max(0.0));
        let mut pulse = Pulse::new(self.beat());
        let mut seen_active = false;
        let mut last_phase: Option<String> = None;
        trace::event("call.wait", json!({ "id": id, "maxS": max, "noWait": no_wait, "own": submitted }));
        loop {
            if let Some(d) = self.result(id) {
                trace::event("call.result", json!({ "id": id, "afterMs": trace::ms_since(started), "exit": d.exit_code, "reason": d.reason, "serverSeconds": d.seconds }));
                self.take(id);
                return Waited::Done(Box::new(d));
            }
            let req_waiting = self.layout.request(id).exists();
            let active: Option<Active> = fsx::read_json(&self.layout.active_file(id));
            if trace::enabled() {
                if let Some(a) = &active {
                    if !seen_active {
                        seen_active = true;
                        trace::event("call.accepted", json!({ "id": id, "afterMs": trace::ms_since(started) }));
                    }
                    let ph = a.phase.clone().or_else(|| a.asks.as_ref().map(|x| format!("ЖДЁТ: {}", x.text)));
                    if ph != last_phase {
                        trace::event("call.progress", json!({ "id": id, "state": a.state, "phase": ph, "afterMs": trace::ms_since(started) }));
                        last_phase = ph;
                    }
                }
            }
            if no_wait && !req_waiting && (active.is_some() || !submitted) {
                return Waited::Pending { id: id.into(), active };
            }
            let silent = pulse.silent(self.alive().as_ref());
            if req_waiting && silent > pulse.dead_after() {
                trace::event("pulse.silent", json!({ "id": id, "silentMs": (silent * 1000.0).round(), "accepted": false }));
                return if submitted { self.give_up(id, true) } else { Waited::ServerSilent { id: id.into(), withdrawn: false } };
            }
            if !req_waiting && silent > SILENT_AFTER_SUBMIT {
                trace::event("pulse.silent", json!({ "id": id, "silentMs": (silent * 1000.0).round(), "accepted": true }));
                return Waited::SilentAfterSubmit { id: id.into() };
            }
            if Instant::now() >= deadline {
                trace::event("call.wait_timeout", json!({ "id": id, "afterMs": trace::ms_since(started), "queued": req_waiting }));
                return if req_waiting && submitted { self.give_up(id, false) } else { Waited::Pending { id: id.into(), active } };
            }
            let elapsed = started.elapsed().as_secs_f64();
            let step = if elapsed < 2.0 { 20 } else if elapsed < 10.0 { 150 } else { 300 };
            std::thread::sleep(Duration::from_millis(step));
        }
    }

    /// Уйти: непринятый запрос отозвать переносом (тот же rename, что у сервера, — гонки нет).
    fn give_up(&self, id: &str, silent: bool) -> Waited {
        match self.withdraw(id) {
            true if silent => Waited::ServerSilent { id: id.into(), withdrawn: true },
            true => Waited::Withdrawn { id: id.into() },
            // не вышло — сервер успел принять: итог будет
            false => match self.result(id) {
                Some(d) => {
                    self.take(id);
                    Waited::Done(Box::new(d))
                }
                None => Waited::Pending { id: id.into(), active: fsx::read_json(&self.layout.active_file(id)) },
            },
        }
    }

    /// Отозвать непринятый запрос. `true` — отозван наверняка; `false` — сервер уже принял.
    pub fn withdraw(&self, id: &str) -> bool {
        let src = self.layout.request(id);
        // запрос отзываем только переносом: удаление не защищает от гонки с rename сервера
        let _ = std::fs::create_dir_all(self.layout.trash());
        let r = fsx::rename_retry(&src, &self.layout.trash().join(format!("req-{id}.json")));
        trace::event("call.withdraw", json!({ "id": id, "withdrawn": r.is_ok(), "err": r.as_ref().err().map(trace::io_err) }));
        r.is_ok()
    }

    /// Итог, если уже есть (файл не трогается).
    pub fn result(&self, id: &str) -> Option<Done> {
        fsx::read_json(&self.layout.done_file(id))
    }

    /// Состояние принятого вызова.
    pub fn active(&self, id: &str) -> Option<Active> {
        fsx::read_json(&self.layout.active_file(id))
    }

    /// «Забрал итог»: удалить, нет права — в корзину. Корзина переполнена — попросить сервер её очистить.
    pub fn take(&self, id: &str) -> Disposed {
        let r = fsx::dispose(&self.layout.done_file(id), &self.layout.trash(), &format!("done-{id}.json"), self.allow_delete);
        if r == Disposed::Trashed {
            self.auto_clean_trash();
        }
        r
    }

    /// Файлов в корзине.
    pub fn trash_count(&self) -> usize {
        std::fs::read_dir(self.layout.trash()).map(|rd| rd.flatten().filter(|e| e.path().is_file()).count()).unwrap_or(0)
    }

    /// Больше 50 файлов — отправить `trash.clean` в фоне, итог не ждать.
    pub fn auto_clean_trash(&self) -> Option<String> {
        if self.trash_count() <= TRASH_AUTO_CLEAN {
            return None;
        }
        // уже отправлен и ещё не взят — второй не нужен
        let pending = fsx::list_json(&self.layout.requests()).iter().any(|p| {
            fsx::read_json::<Request>(p).map(|r| r.command.as_deref() == Some("trash.clean")).unwrap_or(false)
        });
        if pending {
            return None;
        }
        let min_keep = self.channel.as_ref().map(|c| c.limits.min_keep).unwrap_or(60.0);
        let opts = CallOpts { timeout: Some(60.0), keep: Some(min_keep), ..Default::default() };
        trace::event("trash.auto_clean", json!({ "files": self.trash_count() }));
        self.submit("trash.clean", &[], Value::Null, &opts).ok()
    }

    /// Попросить отменить.
    pub fn cancel(&self, id: &str, why: Option<&str>) -> std::io::Result<()> {
        trace::event("call.cancel", json!({ "id": id, "reason": why }));
        fsx::write_json(&self.layout.cancel_file(id), &Cancel { v: V, id: id.into(), reason: why.map(String::from) })
    }

    /// Пути артефактов итога — в локальные.
    pub fn local_paths(&self, d: &Done) -> Vec<PathBuf> {
        d.artifacts.iter().filter_map(|a| self.layout.resolve(self.channel.as_ref(), a)).collect()
    }
}

// ====================================================================== поиск каналов

/// Найденный канал.
#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub title: String,
    pub root: PathBuf,
    /// Откуда известен: `upward`, `index`, `env`.
    pub via: &'static str,
}

/// Найти каналы: вверх от `start` (`channel.json` в папке и в `.efc/`, указатели `.efc-channels.json`), затем по
/// корням из `EFC_PATH` (через `:` или `;`).
pub fn discover(start: &Path) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    let mut d = Some(start.to_path_buf());
    while let Some(dir) = d {
        probe(&dir, "upward", &mut out);
        d = dir.parent().map(Path::to_path_buf);
    }
    if let Ok(v) = std::env::var("EFC_PATH") {
        for root in v.split([':', ';']).filter(|s| !s.is_empty()) {
            let root = PathBuf::from(root);
            probe(&root, "env", &mut out);
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        probe(&e.path(), "env", &mut out);
                    }
                }
            }
        }
    }
    out
}

fn probe(dir: &Path, via: &'static str, out: &mut Vec<Found>) {
    for cand in [dir.to_path_buf(), dir.join(".efc")] {
        add(&cand, None, via, out);
    }
    if let Some(list) = fsx::read_json::<Vec<IndexEntry>>(&dir.join(INDEX_FILE)) {
        for e in list {
            let rel = Path::new(&e.path);
            let root = if rel.is_absolute() { rel.to_path_buf() } else if safe_relative(rel) { dir.join(rel) } else { continue };
            add(&root, Some(&e), "index", out);
        }
    }
}

fn add(root: &Path, entry: Option<&IndexEntry>, via: &'static str, out: &mut Vec<Found>) {
    let ch: Option<Channel> = fsx::read_json(&root.join("channel.json"));
    if ch.is_none() && entry.is_none() {
        return;
    }
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Some(f) = out.iter_mut().find(|f| std::fs::canonicalize(&f.root).unwrap_or_else(|_| f.root.clone()) == canon) {
        // имя канала из указателя главнее имени сервера
        if let Some(e) = entry {
            f.name = e.name.clone();
            if !e.title.is_empty() {
                f.title = e.title.clone();
            }
        }
        return;
    }
    let name = entry.map(|e| e.name.clone()).or(ch.as_ref().map(|c| c.server.clone())).unwrap_or_default();
    let title = entry.map(|e| e.title.clone()).filter(|t| !t.is_empty()).or(ch.as_ref().map(|c| c.title.clone())).unwrap_or_default();
    out.push(Found { name, title, root: root.to_path_buf(), via });
}

/// Выбрать канал: явный путь, имя из найденных или единственный найденный.
pub fn pick(explicit: Option<&str>, start: &Path) -> Result<PathBuf, (i32, String)> {
    if let Some(e) = explicit.map(str::to_string).or_else(|| std::env::var("EFC_CHANNEL").ok()).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(&e);
        if p.join("channel.json").exists() || p.join("alive.json").exists() {
            return Ok(p);
        }
        if p.join(".efc").join("channel.json").exists() {
            return Ok(p.join(".efc"));
        }
        let found = discover(start);
        if let Some(f) = found.iter().find(|f| f.name == e) {
            return Ok(f.root.clone());
        }
        if p.is_dir() {
            return Ok(p); // папка есть, сервера нет — разберётся вызов (код 10)
        }
        return Err((exit::NO_SERVER, format!("Канал «{e}» не найден")));
    }
    let found = discover(start);
    match found.len() {
        0 => Err((exit::NO_SERVER, "Канал не найден: укажите --channel <папка> или EFC_CHANNEL, или запустите efc из папки канала".into())),
        1 => Ok(found[0].root.clone()),
        _ => {
            let names: Vec<String> = found.iter().map(|f| format!("  {}  {}", f.name, f.root.display())).collect();
            Err((exit::AMBIGUOUS, format!("Подходит несколько каналов — выберите --channel <имя|папка>:\n{}", names.join("\n"))))
        }
    }
}
