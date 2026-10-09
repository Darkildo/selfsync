//! Sweeper без постоянного процесса (раздел 5.4): стирает удалённые файлы с истёкшим
//! окном хранения. Запускается systemd-таймером (`notesync sweep`), а в режимах
//! socket/serve — один раз в фоне после первого запроса, если прошлый проход был
//! давно. В CGI не запускается никогда.

use std::time::Duration;

use crate::config::Config;
use crate::db::{now_ms, vault};
use crate::gc::remove_unreferenced;
use crate::state::{AppState, Mode, list_vault_dirs, open_vault};

/// Как часто нужен проход.
pub const SWEEP_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub paths: u64,
    pub blobs: u64,
}

/// Один vault.
pub fn sweep_vault(config: &Config, name: &str, now: i64) -> anyhow::Result<SweepReport> {
    let v = open_vault(config, name, 1)?;
    let mut c = v.pool.get()?;
    let (paths, hashes) = vault::sweep(&mut c, now)?;
    let blobs = remove_unreferenced(&mut c, &v.blobs, &hashes, None)?;
    Ok(SweepReport { paths, blobs })
}

/// Все vault'ы на диске.
pub fn sweep_all(config: &Config) -> anyhow::Result<Vec<(String, SweepReport)>> {
    let now = now_ms();
    let mut out = Vec::new();
    for name in list_vault_dirs(config) {
        let r = sweep_vault(config, &name, now)?;
        out.push((name, r));
    }
    Ok(out)
}

fn due(config: &Config, name: &str, now: i64) -> bool {
    let Ok(v) = open_vault(config, name, 1) else {
        return false;
    };
    let Ok(c) = v.pool.get() else { return false };
    let last = vault::meta_int(&c, "last_sweep")
        .ok()
        .flatten()
        .unwrap_or(0);
    now - last > SWEEP_INTERVAL_MS
}

/// Фоновый проход после первого обслуженного запроса (не в CGI).
pub fn spawn_after_first_request(st: &AppState) {
    if st.mode == Mode::Cgi {
        return;
    }
    let config = st.config.clone();
    tokio::spawn(async move {
        // Дать ответу уйти клиенту.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let res = tokio::task::spawn_blocking(move || {
            let now = now_ms();
            let mut done = Vec::new();
            for name in list_vault_dirs(&config) {
                if due(&config, &name, now) {
                    match sweep_vault(&config, &name, now) {
                        Ok(r) => done.push((name, r)),
                        Err(e) => tracing::warn!(vault = %name, error = %e, "sweep не удался"),
                    }
                }
            }
            done
        })
        .await;
        if let Ok(done) = res {
            for (name, r) in done {
                if r.paths > 0 || r.blobs > 0 {
                    tracing::info!(vault = %name, paths = r.paths, blobs = r.blobs, "sweep");
                }
            }
        }
    });
}
