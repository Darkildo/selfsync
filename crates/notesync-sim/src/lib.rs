//! Детерминированная симуляция синка (раздел 13.2).
//!
//! N клиентов на настоящем [`Engine`] + настоящий серверный код in-process (общий
//! Router через `oneshot`, временная БД) + фейковые ФС, сеть и часы. Seed-управляемый
//! планировщик событий со сбоями: обрыв запроса до или после применения на сервере,
//! убийство клиента в любой момент (в том числе между записью файла и чекпоинтом
//! индекса), сдвиг часов, одновременные правки, удаления и переименования.
//!
//! Инвариант после каждого прогона: каждая правка пользователя (уникальная строка-
//! токен), которую пользователь сам не удалял, присутствует в итоговом vault'е, в
//! конфликтной копии, в корзине клиента или в истории сервера; после успокоения все
//! клиенты сходятся к одному состоянию, совпадающему с сервером.

pub mod fs;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use axum::Router;
use axum::body::Body;
use notesync_core::blob;
use notesync_core::crypto::{self, KdfParams, VaultKeys};
use notesync_core::engine::{Action, Engine, EngineConfig, Event, HttpRequest, IoResult, LogLevel, Notice, SyncState};
use notesync_core::path::{VaultPath, canonical_decode};
use notesync_server::{AppState, Config, Mode, router};
use tower::ServiceExt;

pub use fs::FakeFs;

/// Детерминированный PRNG (SplitMix64).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    pub fn idx(&mut self, n: usize) -> usize {
        usize::try_from(self.below(n as u64)).unwrap_or(0)
    }

    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next_u64() & 0xff) as u8).collect()
    }
}

/// Настройки прогона.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub seed: u64,
    pub clients: usize,
    pub steps: usize,
    /// Вероятность сбоя HTTP-запроса (обрыв до или после сервера).
    pub fault_rate: f64,
    /// Вероятность убийства клиента на шаге.
    pub kill_rate: f64,
    /// Вероятность пользовательской операции на шаге.
    pub user_rate: f64,
    /// Сдвиг часов клиентов, мс.
    pub skew_ms: Vec<i64>,
    /// Регистронезависимая ФС у клиента.
    pub case_insensitive: Vec<bool>,
    /// Включить шифрование на этом шаге (клиент 0).
    pub encrypt_at: Option<usize>,
    /// Пропускать события Obsidian (правки «снаружи», ловятся полным сканом).
    pub skip_event_rate: f64,
}

impl SimConfig {
    pub fn new(seed: u64) -> SimConfig {
        SimConfig {
            seed,
            clients: 2,
            steps: 300,
            fault_rate: 0.05,
            kill_rate: 0.01,
            user_rate: 0.15,
            skew_ms: Vec::new(),
            case_insensitive: Vec::new(),
            encrypt_at: None,
            skip_event_rate: 0.05,
        }
    }

    /// Случайная конфигурация для массовых прогонов.
    pub fn random(seed: u64) -> SimConfig {
        let mut r = Rng::new(seed.wrapping_mul(31).wrapping_add(7));
        let clients = 2 + r.idx(2);
        let mut c = SimConfig::new(seed);
        c.clients = clients;
        c.steps = 150 + r.idx(250);
        c.fault_rate = [0.0, 0.03, 0.1][r.idx(3)];
        c.kill_rate = [0.0, 0.005, 0.02][r.idx(3)];
        c.skew_ms = (0..clients).map(|_| if r.chance(0.2) { 86_400_000 } else { 0 }).collect();
        c.case_insensitive = (0..clients).map(|_| r.chance(0.3)).collect();
        c.encrypt_at = if r.chance(0.15) { Some(r.idx(c.steps)) } else { None };
        c
    }
}

pub const PASSWORD: &str = "correct horse battery staple";

/// Клиент: движок + фейковая ФС + сохранённое состояние.
pub struct Client {
    pub name: String,
    pub token: String,
    pub cfg: EngineConfig,
    pub engine: Option<Engine>,
    pub fs: FakeFs,
    pub saved_index: Option<Vec<u8>>,
    pub remembered_key: Option<Vec<u8>>,
    pub pending: VecDeque<Action>,
    pub skew: i64,
    pub notices: Vec<Notice>,
    pub ui_results: Vec<(u64, notesync_core::engine::UiResult)>,
    pub status: Option<notesync_core::engine::SyncStatus>,
    pub wake: Option<i64>,
    pub kills: u32,
    /// Байты тел HTTP-запросов и ответов (проверка докачки).
    pub sent_bytes: u64,
    pub recv_bytes: u64,
}

/// Учёт токенов для инварианта.
#[derive(Debug, Default, Clone)]
pub struct Ledger {
    next: u64,
    pub written: BTreeSet<String>,
    pub removed: BTreeSet<String>,
}

impl Ledger {
    pub fn fresh(&mut self, client: usize) -> String {
        self.next += 1;
        let t = format!("tok{client}x{}", self.next);
        self.written.insert(t.clone());
        t
    }

    pub fn remove_in(&mut self, text: &[u8]) {
        for t in tokens_in(text) {
            self.removed.insert(t);
        }
    }
}

/// Токены в содержимом (и в тексте, и в «бинарных» файлах).
pub fn tokens_in(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let s = String::from_utf8_lossy(data);
    for w in s.split(|c: char| !c.is_ascii_alphanumeric()) {
        if w.starts_with("tok") && w.contains('x') {
            out.push(w.to_owned());
        }
    }
    out
}

pub struct World {
    pub cfg: SimConfig,
    pub rng: Rng,
    pub rt: tokio::runtime::Runtime,
    pub dir: tempfile::TempDir,
    pub state: AppState,
    pub app: Router,
    pub clients: Vec<Client>,
    pub now: i64,
    pub ledger: Ledger,
    pub trace: VecDeque<String>,
    pub encrypted_password: Option<String>,
    pub step_no: usize,
    pub faults_enabled: bool,
    /// Точечные сбои для сценариев: первый HTTP-запрос, чьё «МЕТОД путь» начинается
    /// с префикса, обрывается до (`false`) или после (`true`) применения на сервере.
    pub planned_faults: VecDeque<(String, bool)>,
}

fn data_root() -> std::path::PathBuf {
    // tmpfs, если есть: fsync блобов на диске сильно замедляет тысячи прогонов.
    let shm = std::path::Path::new("/dev/shm");
    if shm.is_dir() {
        shm.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

impl World {
    pub fn new(cfg: SimConfig) -> World {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let dir = tempfile::Builder::new().prefix("notesync-sim-").tempdir_in(data_root()).expect("tempdir");
        let config = Config::for_tests(dir.path().to_path_buf());
        // CGI-режим: /v1/wait отвечает сразу, без реального ожидания.
        let state = AppState::new(config, Mode::Cgi);
        let app = router(state.clone());
        let mut w = World {
            rng: Rng::new(cfg.seed),
            cfg: cfg.clone(),
            rt,
            dir,
            state,
            app,
            clients: Vec::new(),
            now: 1_791_556_200_000,
            ledger: Ledger::default(),
            trace: VecDeque::new(),
            encrypted_password: None,
            step_no: 0,
            faults_enabled: true,
            planned_faults: VecDeque::new(),
        };
        for i in 0..cfg.clients {
            w.add_client(i);
        }
        w
    }

    pub fn add_client(&mut self, i: usize) {
        let name = format!("dev{i}");
        let token = {
            let pool = self.state.server_pool().expect("server db");
            let c = pool.get().expect("conn");
            notesync_server::db::server::add_device(&c, "sim", &name).expect("device").1
        };
        let ci = self.cfg.case_insensitive.get(i).copied().unwrap_or(false);
        let mut cfg = EngineConfig {
            device_name: name.clone(),
            excludes: vec![".trash/".into()],
            case_insensitive: ci,
            use_wait: false,
            kdf: KdfParams::TEST,
            ..Default::default()
        };
        cfg.hard_excludes = vec![".obsidian/plugins/notesync/".into()];
        let client = Client {
            name,
            token,
            cfg,
            engine: None,
            fs: FakeFs::new(ci),
            saved_index: None,
            remembered_key: None,
            pending: VecDeque::new(),
            skew: self.cfg.skew_ms.get(i).copied().unwrap_or(0),
            notices: Vec::new(),
            ui_results: Vec::new(),
            status: None,
            wake: None,
            kills: 0,
            sent_bytes: 0,
            recv_bytes: 0,
        };
        self.clients.push(client);
        self.start_client(self.clients.len() - 1);
    }

    pub fn log(&mut self, s: String) {
        let limit = if std::env::var_os("SIM_FULL_TRACE").is_some() { usize::MAX } else { 400 };
        if self.trace.len() > limit {
            self.trace.pop_front();
        }
        self.trace.push_back(format!("[{}] {s}", self.step_no));
    }

    pub fn client_now(&self, c: usize) -> i64 {
        self.now + self.clients[c].skew
    }

    /// Передать событие движку клиента.
    pub fn deliver(&mut self, c: usize, ev: Event) {
        let now = self.client_now(c);
        let Some(engine) = self.clients[c].engine.as_mut() else {
            return;
        };
        let actions = engine.handle(now, ev);
        for a in actions {
            self.absorb(c, a);
        }
    }

    /// Действия без ответа обрабатываются сразу, остальные — в очередь.
    fn absorb(&mut self, c: usize, a: Action) {
        match a {
            Action::Wake { at } => self.clients[c].wake = Some(at),
            Action::Status { status } => self.clients[c].status = Some(status),
            Action::Notify { notice } => {
                self.log(format!("c{c} notice {notice:?}"));
                self.clients[c].notices.push(notice);
            }
            Action::Log { level, message } => {
                if !matches!(level, LogLevel::Debug) || std::env::var_os("SIM_FULL_TRACE").is_some() {
                    self.log(format!("c{c} log {message}"));
                }
            }
            Action::RememberKey { key } => self.clients[c].remembered_key = Some(key),
            Action::ForgetKey => self.clients[c].remembered_key = None,
            Action::UiResult { req, result } => self.clients[c].ui_results.push((req, result)),
            other => self.clients[c].pending.push_back(other),
        }
    }

    pub fn start_client(&mut self, c: usize) {
        let cl = &mut self.clients[c];
        cl.engine = Some(Engine::new(cl.cfg.clone(), cl.saved_index.as_deref()));
        cl.pending.clear();
        let key = cl.remembered_key.clone();
        self.deliver(c, Event::Start { key });
    }

    /// Убить клиента (в любой момент) и запустить заново с последнего чекпоинта.
    pub fn kill(&mut self, c: usize) {
        self.log(format!("c{c} KILL"));
        self.clients[c].kills += 1;
        self.clients[c].engine = None;
        self.clients[c].pending.clear();
        self.start_client(c);
    }

    fn http(&mut self, c: usize, req: HttpRequest) -> IoResult {
        let mut b = http::Request::builder().method(req.method.as_str()).uri(&req.path);
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        if req.auth {
            b = b.header("authorization", format!("Bearer {}", self.clients[c].token));
        }
        self.clients[c].sent_bytes += req.body.len() as u64;
        let Ok(request) = b.body(Body::from(req.body)) else {
            return IoResult::Failed {
                message: "bad request".into(),
            };
        };
        let app = self.app.clone();
        let (status, headers, body) = self.rt.block_on(async move {
            let resp = app.oneshot(request).await.expect("infallible");
            let status = resp.status().as_u16();
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                .collect();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("body");
            (status, headers, body.to_vec())
        });
        self.clients[c].recv_bytes += body.len() as u64;
        IoResult::Http { status, headers, body }
    }

    /// Выполнить одно действие клиента (с возможным сбоем).
    fn exec(&mut self, c: usize, a: Action) -> Option<(u64, IoResult)> {
        let now = self.client_now(c);
        let id = a.id()?;
        let res = match a {
            Action::Http { req, .. } => {
                let line = format!("{} {}", req.method, req.path);
                let planned = self.planned_faults.iter().position(|(p, _)| line.starts_with(p.as_str()));
                let fault = if let Some(i) = planned {
                    self.planned_faults.remove(i).map(|(_, after)| !after)
                } else if self.faults_enabled && self.cfg.fault_rate > 0.0 && self.rng.chance(self.cfg.fault_rate) {
                    Some(self.rng.chance(0.5))
                } else {
                    None
                };
                match fault {
                    Some(true) => {
                        self.log(format!("c{c} HTTP DROP-BEFORE {} {}", req.method, req.path));
                        IoResult::Failed {
                            message: "connection reset".into(),
                        }
                    }
                    Some(false) => {
                        let (m, p) = (req.method.clone(), req.path.clone());
                        let _ = self.http(c, req);
                        self.log(format!("c{c} HTTP DROP-AFTER {m} {p}"));
                        IoResult::Failed {
                            message: "connection reset after send".into(),
                        }
                    }
                    None => {
                        let (m, p) = (req.method.clone(), req.path.clone());
                        let r = self.http(c, req);
                        if let IoResult::Http { status, .. } = &r {
                            self.log(format!("c{c} HTTP {m} {p} -> {status}"));
                        }
                        r
                    }
                }
            }
            Action::List { .. } => self.clients[c].fs.list(),
            Action::Stat { path, .. } => self.clients[c].fs.stat(&path),
            Action::Read { path, offset, len, .. } => self.clients[c].fs.read(&path, offset, len),
            Action::Write { path, data, expect, .. } => {
                let r = self.clients[c].fs.write(&path, data, &expect, now);
                self.log(format!("c{c} write {path} -> {}", short(&r)));
                r
            }
            Action::WriteTemp { temp, offset, data, .. } => self.clients[c].fs.write_temp(&temp, offset, &data),
            Action::ReadTemp { temp, offset, len, .. } => self.clients[c].fs.read_temp(&temp, offset, len),
            Action::CommitTemp { temp, path, expect, .. } => {
                let r = self.clients[c].fs.commit_temp(&temp, &path, &expect, now);
                self.log(format!("c{c} commit {path} -> {}", short(&r)));
                r
            }
            Action::DeleteTemp { temp, .. } => {
                self.clients[c].fs.temps.remove(&temp);
                IoResult::Done
            }
            Action::Trash { path, expect, .. } => {
                let r = self.clients[c].fs.trash(&path, &expect);
                self.log(format!("c{c} trash {path} -> {}", short(&r)));
                r
            }
            Action::Rename { from, to, .. } => {
                let r = self.clients[c].fs.rename_io(&from, &to, now);
                self.log(format!("c{c} rename {from} -> {to}: {}", short(&r)));
                r
            }
            Action::Mkdir { path, .. } => self.clients[c].fs.mkdir(&path),
            Action::Rmdir { path, .. } => self.clients[c].fs.rmdir(&path),
            Action::SaveIndex { data, .. } => {
                self.clients[c].saved_index = Some(data);
                IoResult::Done
            }
            Action::CacheRead { key, .. } => match self.clients[c].fs.cache.get(&key) {
                Some(d) => IoResult::Data { data: d.clone() },
                None => IoResult::NotFound,
            },
            Action::CacheWrite { key, data, .. } => {
                self.clients[c].fs.cache.insert(key, data);
                IoResult::Done
            }
            Action::CacheDelete { key, .. } => {
                self.clients[c].fs.cache.remove(&key);
                IoResult::Done
            }
            _ => return None,
        };
        Some((id, res))
    }

    /// Обработать одно ожидающее действие клиента. `false` — очередь пуста.
    pub fn pump(&mut self, c: usize) -> bool {
        if self.clients[c].engine.is_none() {
            return false;
        }
        let n = self.clients[c].pending.len();
        if n == 0 {
            return false;
        }
        // Чаще по порядку, иногда — вразнобой (ответы параллельных задач).
        let i = if n > 1 && self.rng.chance(0.2) { self.rng.idx(n.min(4)) } else { 0 };
        let Some(a) = self.clients[c].pending.remove(i) else {
            return false;
        };
        if let Some((id, result)) = self.exec(c, a) {
            self.deliver(c, Event::Done { id, result });
        }
        true
    }

    /// Выполнить первое ожидающее действие клиента строго по порядку. Возвращает
    /// его описание («HTTP PUT /v1/...», «Write a.md», «SaveIndex»…).
    pub fn pump_one(&mut self, c: usize) -> Option<String> {
        self.clients[c].engine.as_ref()?;
        let a = self.clients[c].pending.pop_front()?;
        let label = match &a {
            Action::Http { req, .. } => format!("HTTP {} {}", req.method, req.path),
            Action::Write { path, .. } => format!("Write {path}"),
            Action::CommitTemp { path, .. } => format!("CommitTemp {path}"),
            Action::Rename { from, to, .. } => format!("Rename {from} -> {to}"),
            Action::Trash { path, .. } => format!("Trash {path}"),
            Action::SaveIndex { .. } => "SaveIndex".to_owned(),
            other => format!("{other:?}").split([' ', '{']).next().unwrap_or("").to_owned(),
        };
        if let Some((id, result)) = self.exec(c, a) {
            self.deliver(c, Event::Done { id, result });
        }
        Some(label)
    }

    /// Доставить таймеры, если подошло время.
    pub fn tick(&mut self, c: usize) {
        if let Some(at) = self.clients[c].wake
            && at <= self.client_now(c) {
                self.clients[c].wake = None;
                self.deliver(c, Event::Tick);
            }
    }

    /// Прогнать клиента, пока у него есть действия (без продвижения времени).
    pub fn drain(&mut self, c: usize) {
        for _ in 0..200_000 {
            if !self.pump(c) {
                break;
            }
        }
    }

    /// Синк клиента «сейчас» до конца.
    pub fn sync(&mut self, c: usize) {
        self.deliver(c, Event::SyncNow);
        self.drain(c);
        self.answer_password(c);
        self.drain(c);
    }

    fn answer_password(&mut self, c: usize) {
        let need = self.clients[c]
            .status
            .as_ref()
            .is_some_and(|s| s.state == SyncState::NeedPassword);
        if need
            && let Some(pw) = self.encrypted_password.clone() {
                self.deliver(c, Event::Password { password: pw, remember: true });
                self.drain(c);
                self.deliver(c, Event::SyncNow);
                self.drain(c);
            }
    }

    // -------------------------------------------------------------------------
    // Действия пользователя (как в Obsidian: событие на каждую правку)
    // -------------------------------------------------------------------------

    fn emit(&mut self, c: usize, ev: Event) {
        if self.cfg.skip_event_rate > 0.0 && self.rng.chance(self.cfg.skip_event_rate) {
            return;
        }
        self.deliver(c, ev);
    }

    pub fn user_write(&mut self, c: usize, path: &str, data: Vec<u8>) {
        let now = self.client_now(c);
        if let Some(old) = self.clients[c].fs.read_file(path).cloned() {
            // Пользователь целиком заменил содержимое: старые токены убраны сознательно.
            let new_tokens: BTreeSet<String> = tokens_in(&data).into_iter().collect();
            for t in tokens_in(&old) {
                if !new_tokens.contains(&t) {
                    self.ledger.removed.insert(t);
                }
            }
        }
        self.log(format!("c{c} USER write {path} ({} b) {:?}", data.len(), tokens_in(&data)));
        self.clients[c].fs.user_write(path, data, now);
        self.emit(c, Event::Changed { path: path.to_owned() });
    }

    pub fn user_delete(&mut self, c: usize, path: &str) {
        self.log(format!("c{c} USER delete {path}"));
        self.clients[c].fs.user_delete(path);
        self.emit(c, Event::Deleted { path: path.to_owned() });
    }

    pub fn user_rename(&mut self, c: usize, from: &str, to: &str) -> bool {
        let now = self.client_now(c);
        if !self.clients[c].fs.user_rename(from, to, now) {
            return false;
        }
        self.log(format!("c{c} USER rename {from} -> {to}"));
        self.emit(c, Event::Renamed { from: from.to_owned(), to: to.to_owned() });
        true
    }

    /// Правка текстового файла: добавить строку, вставить, заменить или удалить.
    pub fn user_edit(&mut self, c: usize, path: &str) {
        let Some(old) = self.clients[c].fs.read_file(path).cloned() else { return };
        if !blob::is_text(&old) {
            let tok = self.ledger.fresh(c);
            let mut b = self.rng.bytes(64);
            b.push(0);
            b.extend_from_slice(tok.as_bytes());
            self.user_write(c, path, b);
            return;
        }
        let text = String::from_utf8_lossy(&old).to_string();
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let tok = self.ledger.fresh(c);
        match self.rng.idx(4) {
            0 => lines.push(format!("line {tok}")),
            1 => {
                let i = self.rng.idx(lines.len() + 1);
                lines.insert(i, format!("ins {tok}"));
            }
            2 if !lines.is_empty() => {
                let i = self.rng.idx(lines.len());
                self.ledger.remove_in(lines[i].as_bytes());
                lines[i] = format!("rep {tok}");
            }
            3 if lines.len() > 1 => {
                let i = self.rng.idx(lines.len());
                self.ledger.remove_in(lines[i].as_bytes());
                lines.remove(i);
                lines.push(format!("tail {tok}"));
            }
            _ => lines.push(format!("line {tok}")),
        }
        let mut out = lines.join("\n");
        out.push('\n');
        let now = self.client_now(c);
        self.log(format!("c{c} USER edit {path} +{tok}"));
        self.clients[c].fs.user_write(path, out.into_bytes(), now);
        self.emit(c, Event::Changed { path: path.to_owned() });
    }

    fn files_of(&self, c: usize) -> Vec<String> {
        self.clients[c]
            .fs
            .snapshot()
            .into_keys()
            .filter(|p| !p.starts_with(".trash"))
            .collect()
    }

    /// Случайная операция пользователя на клиенте.
    pub fn random_user_op(&mut self, c: usize) {
        let files = self.files_of(c);
        const NAMES: [&str; 8] = ["a.md", "b.md", "notes/c.md", "notes/d.md", "Cafe\u{301}.md", "img/p.png", "deep/x/y.md", "e.md"];
        let roll = self.rng.idx(100);
        if files.is_empty() || roll < 15 {
            let name = NAMES[self.rng.idx(NAMES.len())];
            if self.clients[c].fs.get(name).is_some() {
                self.user_edit(c, name);
                return;
            }
            let tok = self.ledger.fresh(c);
            let data = if name.ends_with(".png") {
                let mut b = self.rng.bytes(200);
                b.push(0);
                b.extend_from_slice(tok.as_bytes());
                b
            } else {
                format!("# {name}\nfirst {tok}\nsecond line\nthird line\n").into_bytes()
            };
            self.user_write(c, name, data);
            return;
        }
        let f = files[self.rng.idx(files.len())].clone();
        match roll {
            15..=74 => self.user_edit(c, &f),
            75..=82 => self.user_delete(c, &f),
            83..=92 => {
                // Переименование: иногда только регистр.
                let vp = VaultPath::normalize(&f).ok();
                let to = if self.rng.chance(0.3) {
                    let mut chars: Vec<char> = f.chars().collect();
                    if let Some(i) = chars.iter().rposition(|c| c.is_alphabetic()) {
                        chars[i] = if chars[i].is_lowercase() {
                            chars[i].to_uppercase().next().unwrap_or(chars[i])
                        } else {
                            chars[i].to_lowercase().next().unwrap_or(chars[i])
                        };
                    }
                    chars.into_iter().collect()
                } else {
                    let base = vp.as_ref().map_or("x.md".to_owned(), |p| p.file_name().to_owned());
                    let dir = ["", "moved/", "notes/"][self.rng.idx(3)];
                    format!("{dir}r{}-{base}", self.rng.below(5))
                };
                self.user_rename(c, &f, &to);
            }
            _ => {
                // Пустая папка.
                let d = format!("empty{}", self.rng.below(3));
                self.clients[c].fs.user_mkdir(&d);
                self.emit(c, Event::Changed { path: d });
            }
        }
    }

    /// Один шаг планировщика.
    pub fn step(&mut self) {
        self.step_no += 1;
        self.now += i64::try_from(self.rng.below(400)).unwrap_or(0);
        if let Some(at) = self.cfg.encrypt_at
            && at == self.step_no && self.encrypted_password.is_none() {
                self.log("ENABLE ENCRYPTION on c0".into());
                self.encrypted_password = Some(PASSWORD.to_owned());
                let remember = self.rng.chance(0.5);
                self.deliver(0, Event::EnableEncryption { password: PASSWORD.into(), remember });
            }
        let c = self.rng.idx(self.clients.len());
        let roll = self.rng.next_u64() % 1000;
        if (roll as f64) < self.cfg.kill_rate * 1000.0 {
            self.kill(c);
            return;
        }
        if self.rng.chance(self.cfg.user_rate) {
            self.random_user_op(c);
            return;
        }
        if self.rng.chance(0.03) {
            self.deliver(c, Event::SyncNow);
            return;
        }
        if self.rng.chance(0.02) {
            let ev = if self.rng.chance(0.5) { Event::Hidden } else { Event::Visible };
            self.deliver(c, ev);
            return;
        }
        self.tick(c);
        // Несколько действий подряд.
        for _ in 0..1 + self.rng.idx(6) {
            if !self.pump(c) {
                break;
            }
        }
        self.answer_password_maybe(c);
    }

    fn answer_password_maybe(&mut self, c: usize) {
        if self.rng.chance(0.3) {
            let need = self.clients[c]
                .status
                .as_ref()
                .is_some_and(|s| s.state == SyncState::NeedPassword);
            if need
                && let Some(pw) = self.encrypted_password.clone() {
                    let remember = self.rng.chance(0.5);
                    self.deliver(c, Event::Password { password: pw, remember });
                }
        }
    }

    pub fn run(&mut self) {
        for _ in 0..self.cfg.steps {
            self.step();
        }
    }

    /// Успокоение: без сбоев гоняем синк на всех клиентах, пока ничего не меняется.
    pub fn settle(&mut self) -> Result<(), String> {
        self.faults_enabled = false;
        for c in 0..self.clients.len() {
            if self.clients[c].engine.is_none() {
                self.start_client(c);
            }
            self.deliver(c, Event::Visible);
        }
        let mut last = String::new();
        let mut prev_snap = self.snapshots();
        let mut churn = String::new();
        for round in 0..30 {
            self.now += 20_000;
            for c in 0..self.clients.len() {
                self.sync(c);
            }
            let sig = self.signature();
            let pending = self.clients.iter().any(|cl| cl.engine.as_ref().is_some_and(Engine::has_pending));
            if sig == last && !pending && round > 0 {
                return Ok(());
            }
            let snap = self.snapshots();
            if round >= 27 {
                churn.push_str(&format!("\nраунд {round}: seq {}, pending {pending}", self.server_seq()));
                for (c, (a, b)) in prev_snap.iter().zip(&snap).enumerate() {
                    let changed: Vec<&String> = a.keys().chain(b.keys()).filter(|k| a.get(*k) != b.get(*k)).collect::<BTreeSet<_>>().into_iter().collect();
                    if !changed.is_empty() {
                        churn.push_str(&format!("\n  c{c} меняются {changed:?}"));
                    }
                    if let Some(e) = self.clients[c].engine.as_ref() {
                        let idx = e.index();
                        let pend: Vec<&String> = idx
                            .files
                            .iter()
                            .filter(|(_, f)| f.content_dirty() || f.delete_pending() || f.server_path.is_some() || (f.folder && f.base_rev == 0 && f.local.is_some()))
                            .map(|(k, _)| k)
                            .collect();
                        if !pend.is_empty() {
                            churn.push_str(&format!("\n  c{c} ждут отправки {pend:?}"));
                        }
                    }
                }
            }
            prev_snap = snap;
            last = sig;
        }
        let conv = self.check_converged().err().unwrap_or_default();
        Err(format!("не сошлось за 30 раундов{churn}\n{conv}"))
    }

    fn snapshots(&self) -> Vec<BTreeMap<String, Vec<u8>>> {
        self.clients.iter().map(|c| c.fs.snapshot()).collect()
    }

    /// Отпечаток состояния: ФС всех клиентов + seq сервера.
    fn signature(&self) -> String {
        let mut s = String::new();
        for cl in &self.clients {
            for (p, d) in cl.fs.snapshot() {
                s.push_str(&format!("{p}:{};", notesync_core::hash::Hash::of(&d).to_hex()));
            }
            s.push('|');
        }
        s.push_str(&format!("{}", self.server_seq()));
        s
    }

    pub fn server_seq(&self) -> u64 {
        let v = self.state.vault("sim").expect("vault");
        let c = v.pool.get().expect("conn");
        notesync_server::db::vault::current_seq(&c).unwrap_or(0)
    }

    /// Ключи vault'а (если он зашифрован) — из записи на сервере и пароля.
    pub fn vault_keys(&self) -> Option<VaultKeys> {
        let pw = self.encrypted_password.as_ref()?;
        let v = self.state.vault("sim").ok()?;
        let c = v.pool.get().ok()?;
        let rec = notesync_server::db::vault::meta_blob(&c, "vault_key").ok()??;
        crypto::open_master_key(&rec, pw).ok().map(|m| m.derive())
    }

    /// Живое состояние сервера: путь → открытый текст.
    pub fn server_files(&self) -> BTreeMap<String, Vec<u8>> {
        let keys = self.vault_keys();
        let v = self.state.vault("sim").expect("vault");
        let c = v.pool.get().expect("conn");
        let (entries, _) = notesync_server::db::vault::changes(&c, 0, 1_000_000).expect("changes");
        let mut out = BTreeMap::new();
        for e in entries {
            if e.deleted || e.folder {
                continue;
            }
            let Some(p) = e.path.as_ref() else { continue };
            let path = if p.encrypted {
                match keys.as_ref().and_then(|k| k.decrypt_path(p).ok()) {
                    Some(vp) => vp.as_str().to_owned(),
                    None => continue,
                }
            } else {
                match VaultPath::from_segments(&p.segments) {
                    Ok(vp) => vp.as_str().to_owned(),
                    Err(_) => continue,
                }
            };
            let Some(h) = notesync_core::hash::Hash::from_slice(&e.hash) else { continue };
            let Ok(data) = std::fs::read(v.blobs.path_of(&h)) else { continue };
            if let Ok(plain) = blob::decode(&data, keys.as_ref()) {
                out.insert(path, plain);
            }
        }
        out
    }

    /// Всё содержимое, которое когда-либо было на сервере (история).
    pub fn server_history_texts(&self) -> Vec<Vec<u8>> {
        let keys = self.vault_keys();
        let v = self.state.vault("sim").expect("vault");
        let c = v.pool.get().expect("conn");
        let mut st = c
            .prepare("SELECT hash FROM revisions WHERE hash IS NOT NULL UNION SELECT hash FROM files WHERE hash IS NOT NULL")
            .expect("prepare");
        let hashes: Vec<Vec<u8>> = st
            .query_map([], |r| r.get(0))
            .expect("query")
            .filter_map(Result::ok)
            .collect();
        let mut out = Vec::new();
        for h in hashes {
            let Some(hh) = notesync_core::hash::Hash::from_slice(&h) else { continue };
            let Ok(data) = std::fs::read(v.blobs.path_of(&hh)) else { continue };
            if let Ok(p) = blob::decode(&data, keys.as_ref()) {
                out.push(p);
            } else if let Ok(p) = blob::decode(&data, None) {
                out.push(p);
            }
        }
        let _ = canonical_decode;
        out
    }

    /// Главный инвариант: ни одна правка не исчезла молча.
    pub fn check_no_loss(&self) -> Result<(), String> {
        let mut found: BTreeSet<String> = BTreeSet::new();
        for cl in &self.clients {
            for d in cl.fs.snapshot().values() {
                found.extend(tokens_in(d));
            }
            for (_, d) in &cl.fs.trash {
                found.extend(tokens_in(d));
            }
        }
        for d in self.server_history_texts() {
            found.extend(tokens_in(&d));
        }
        let lost: Vec<&String> = self
            .ledger
            .written
            .iter()
            .filter(|t| !self.ledger.removed.contains(*t) && !found.contains(*t))
            .collect();
        if lost.is_empty() {
            Ok(())
        } else {
            Err(format!("потеряны правки: {lost:?}"))
        }
    }

    /// Сходимость: все клиенты видят одно и то же, и это совпадает с сервером.
    pub fn check_converged(&self) -> Result<(), String> {
        let server = self.server_files();
        let norm = |m: BTreeMap<String, Vec<u8>>| -> BTreeMap<String, Vec<u8>> {
            m.into_iter().filter(|(p, _)| !p.starts_with(".trash")).collect()
        };
        for (i, cl) in self.clients.iter().enumerate() {
            let local = norm(cl.fs.snapshot());
            if local != server {
                let only_local: Vec<_> = local.keys().filter(|k| !server.contains_key(*k)).collect();
                let only_server: Vec<_> = server.keys().filter(|k| !local.contains_key(*k)).collect();
                let differ: Vec<_> = local.iter().filter(|(k, v)| server.get(*k).is_some_and(|s| s != *v)).map(|x| x.0).collect();
                return Err(format!(
                    "клиент {i} не совпадает с сервером: только локально {only_local:?}, только на сервере {only_server:?}, различаются {differ:?}"
                ));
            }
        }
        Ok(())
    }

    pub fn dump_trace(&self) -> String {
        self.trace.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

fn short(r: &IoResult) -> String {
    match r {
        IoResult::Stat { .. } => "ok".into(),
        other => format!("{other:?}").chars().take(40).collect(),
    }
}

/// Полный прогон по seed: шаги со сбоями, успокоение, проверка инвариантов.
pub fn run_seed(cfg: SimConfig) -> Result<(), String> {
    let mut w = World::new(cfg.clone());
    w.run();
    let settled = w.settle();
    let res = settled.and_then(|()| w.check_no_loss()).and_then(|()| w.check_converged());
    res.map_err(|e| format!("seed {}: {e}\n--- конфигурация: {cfg:?}\n--- последние события:\n{}", cfg.seed, w.dump_trace()))
}
