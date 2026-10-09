//! SQLite: открытие с PRAGMA, пулы соединений, миграции через `PRAGMA user_version`.

pub mod server;
pub mod vault;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, TransactionBehavior};
use scheduled_thread_pool::{OnPoolDropBehavior, ScheduledThreadPool};

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

/// PRAGMA при каждом открытии. `busy_timeout` первым: переключение в WAL при
/// одновременном старте нескольких CGI-процессов тоже должно ждать, а не падать.
/// `secure_delete`: удалённые строки затираются нулями, а не остаются в свободных
/// страницах — иначе открытые имена файлов переживали бы миграцию на шифрование.
const PRAGMAS: &str = "PRAGMA busy_timeout = 5000;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA secure_delete = ON;";

pub fn init_conn(c: &mut Connection) -> rusqlite::Result<()> {
    c.execute_batch(PRAGMAS)
}

/// Перенести WAL в базу и обрезать его до нуля. В WAL лежат прежние версии страниц
/// — с тем, что только что удалено (открытый текст после миграции, окончательно
/// стёртые файлы); `secure_delete` их не касается. Ждёт читателей до `busy_timeout`.
pub fn truncate_wal(c: &Connection) -> rusqlite::Result<()> {
    c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
}

/// Открывает одно соединение (служебные команды).
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let mut c = Connection::open(path)?;
    init_conn(&mut c)?;
    Ok(c)
}

/// Пул соединений к файлу БД. Соединения создаются лениво.
///
/// Планировщик фоновых задач пула (открытие соединений, уборка простаивающих) по
/// умолчанию при уничтожении пула ждёт запланированного запуска уборки — до 30 с
/// живут три потока на пул. Здесь отложенные задачи отбрасываются, и потоки
/// завершаются сразу: важно, когда пулы открываются и закрываются часто.
pub fn pool(path: &Path, max_size: u32) -> Pool {
    let workers = ScheduledThreadPool::builder()
        .num_threads(3)
        .thread_name_pattern("r2d2-worker-{}")
        .on_drop_behavior(OnPoolDropBehavior::DiscardPendingScheduled)
        .build();
    let manager = SqliteConnectionManager::file(path).with_init(init_conn);
    r2d2::Pool::builder()
        .thread_pool(Arc::new(workers))
        .max_size(max_size)
        .min_idle(Some(0))
        .idle_timeout(Some(Duration::from_secs(300)))
        .connection_timeout(Duration::from_secs(30))
        .build_unchecked(manager)
}

/// Миграция: список SQL-скриптов, i-й переводит схему с версии i на i+1.
/// Идемпотентна: версия перепроверяется внутри `BEGIN IMMEDIATE`, так что
/// конкурентные процессы не применят шаг дважды.
pub fn migrate(c: &mut Connection, steps: &[&str]) -> rusqlite::Result<()> {
    let target = i64::try_from(steps.len()).unwrap_or(i64::MAX);
    let current: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current >= target {
        return Ok(());
    }
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, step) in steps.iter().enumerate() {
        let v = i64::try_from(i).unwrap_or(i64::MAX);
        if v >= current {
            tx.execute_batch(step)?;
        }
    }
    if current < target {
        tx.pragma_update(None, "user_version", target)?;
    }
    tx.commit()
}

/// Текущее время, мс.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// i64 из SQLite в u64 протокола (отрицательных значений в схеме нет).
pub fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// u64 протокола в i64 для SQLite.
pub fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}
