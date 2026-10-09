//! Общее состояние процесса. Между запросами сервер ничего не помнит в памяти,
//! кроме кэша открытых пулов и счётчиков простоя: любой процесс можно убить в
//! любой момент (CGI, запуск по требованию).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::Connection;
use tokio::sync::watch;

use crate::blobs::BlobStore;
use crate::config::Config;
use crate::db::{self, Pool, now_ms};
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Процесс на каждый запрос: никаких фоновых задач, `/v1/wait` отвечает сразу.
    Cgi,
    /// systemd socket activation с выходом по простою.
    Socket,
    /// Обычный процесс.
    Serve,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

pub struct Inner {
    pub config: Config,
    pub mode: Mode,
    server_db: OnceLock<Pool>,
    vaults: Mutex<HashMap<String, Arc<Vault>>>,
    pub activity: Activity,
    /// Сигнал завершения: ожидающие `/v1/wait` отвечают сразу.
    pub shutdown: watch::Sender<bool>,
    /// Первый запрос обслужен (после него — фоновый sweeper, раздел 5.4).
    pub first_request_done: AtomicBool,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// Учёт активности для выхода по простою.
#[derive(Default)]
pub struct Activity {
    pub in_flight: AtomicUsize,
    pub last: AtomicI64,
}

impl Activity {
    pub fn touch(&self) {
        self.last.store(now_ms(), Ordering::Relaxed);
    }
}

/// Открытый vault.
pub struct Vault {
    pub name: String,
    pub dir: PathBuf,
    pub pool: Pool,
    pub blobs: BlobStore,
    /// Последний известный `seq` — будит ожидающие `/v1/wait` этого процесса.
    pub notify: watch::Sender<u64>,
}

impl Vault {
    /// Выполняет работу с БД vault'а в blocking-пуле.
    pub async fn db<T, F>(self: &Arc<Self>, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &Vault) -> ApiResult<T> + Send + 'static,
    {
        let v = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut c = v.pool.get()?;
            f(&mut c, &v)
        })
        .await?
    }
}

impl AppState {
    pub fn new(config: Config, mode: Mode) -> AppState {
        let (shutdown, _) = watch::channel(false);
        let state = AppState(Arc::new(Inner {
            config,
            mode,
            server_db: OnceLock::new(),
            vaults: Mutex::new(HashMap::new()),
            activity: Activity::default(),
            shutdown,
            first_request_done: AtomicBool::new(false),
        }));
        state.activity.touch();
        state
    }

    fn pool_size(&self) -> u32 {
        match self.mode {
            Mode::Cgi => 2,
            Mode::Socket | Mode::Serve => 8,
        }
    }

    /// Пул `server.db` (создаётся и мигрирует при первом обращении).
    pub fn server_pool(&self) -> anyhow::Result<Pool> {
        if let Some(p) = self.server_db.get() {
            return Ok(p.clone());
        }
        std::fs::create_dir_all(&self.config.data_dir)?;
        let path = self.config.data_dir.join("server.db");
        let pool = db::pool(&path, self.pool_size());
        {
            let mut c = pool.get()?;
            db::server::migrate_server(&mut c)?;
        }
        Ok(self.server_db.get_or_init(|| pool).clone())
    }

    /// Работа с `server.db` в blocking-пуле.
    pub async fn server_db<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> ApiResult<T> + Send + 'static,
    {
        let st = self.clone();
        tokio::task::spawn_blocking(move || {
            let pool = st.server_pool()?;
            let mut c = pool.get()?;
            f(&mut c)
        })
        .await?
    }

    /// Открывает vault (каталог, схема) или берёт уже открытый.
    pub fn vault(&self, name: &str) -> ApiResult<Arc<Vault>> {
        if !db::server::valid_vault_name(name) {
            return Err(ApiError::bad_request("bad_vault", "неверное имя vault'а"));
        }
        if let Some(v) = self
            .vaults
            .lock()
            .map_err(|_| ApiError::internal("poisoned"))?
            .get(name)
        {
            return Ok(Arc::clone(v));
        }
        let v = Arc::new(open_vault(&self.config, name, self.pool_size())?);
        let mut map = self
            .vaults
            .lock()
            .map_err(|_| ApiError::internal("poisoned"))?;
        Ok(Arc::clone(map.entry(name.to_owned()).or_insert(v)))
    }

    /// Уже открытые vault'ы (для checkpoint при завершении).
    pub fn open_vaults(&self) -> Vec<Arc<Vault>> {
        self.vaults
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// WAL checkpoint всех открытых БД (перед выходом).
    pub fn checkpoint(&self) {
        for v in self.open_vaults() {
            if let Ok(c) = v.pool.get() {
                let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            }
        }
        if let Some(p) = self.server_db.get()
            && let Ok(c) = p.get()
        {
            let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
    }
}

/// Каталог vault'а.
pub fn vault_dir(config: &Config, name: &str) -> PathBuf {
    config.data_dir.join("vaults").join(name)
}

pub fn open_vault(config: &Config, name: &str, pool_size: u32) -> anyhow::Result<Vault> {
    let dir = vault_dir(config, name);
    let blobs = BlobStore::new(&dir);
    blobs.ensure_dirs()?;
    let pool = db::pool(&dir.join("meta.db"), pool_size);
    let seq = {
        let mut c = pool.get()?;
        db::vault::migrate_vault(&mut c)?;
        db::vault::current_seq(&c)?
    };
    let (notify, _) = watch::channel(seq);
    Ok(Vault {
        name: name.to_owned(),
        dir,
        pool,
        blobs,
        notify,
    })
}

/// Имена vault'ов по каталогам на диске.
pub fn list_vault_dirs(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(config.data_dir.join("vaults")) {
        for e in rd.flatten() {
            if let Some(n) = e.file_name().to_str()
                && db::server::valid_vault_name(n)
                && e.path().join("meta.db").is_file()
            {
                out.push(n.to_owned());
            }
        }
    }
    out.sort();
    out
}
