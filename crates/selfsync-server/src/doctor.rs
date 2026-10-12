//! `selfsync doctor` (F15): проверка установки — каталог данных и место на диске,
//! базы, внешний адрес и прокси, таймеры, бэкапы на другом диске, итог `verify`,
//! квоты. Только читает. Отказ — то, из-за чего синк не работает или данные под
//! угрозой; предупреждение — то, что стоит поправить.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::db::server;
use crate::db::vault::meta_int;
use crate::state::{list_vault_dirs, open_vault};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    /// Не проверялось (неприменимо).
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    /// Что проверялось: «каталог данных», «vault notes», …
    pub what: String,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.findings.iter().any(|f| f.level == Level::Fail)
    }

    fn add(&mut self, level: Level, what: impl Into<String>, text: impl Into<String>) {
        self.findings.push(Finding {
            level,
            what: what.into(),
            text: text.into(),
        });
    }
}

pub struct Options {
    /// Каталог `selfsync backup --rotate` (таймер `selfsync-backup.timer`).
    pub backup_dir: PathBuf,
    /// Проверять таймеры systemd (если система под systemd).
    pub systemd: bool,
    /// Корень для файлов systemd (тесты); обычно `/`.
    pub root: PathBuf,
    /// Запрос к внешнему адресу через `curl` (если он есть).
    pub external: bool,
    pub now_ms: i64,
}

impl Options {
    pub fn new(now_ms: i64) -> Options {
        Options {
            backup_dir: PathBuf::from("/var/lib/selfsync-backups"),
            systemd: true,
            root: PathBuf::from("/"),
            external: true,
            now_ms,
        }
    }
}

const GIB: u64 = 1 << 30;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// `selfsync-verify.timer` — раз в неделю; дольше — таймер не работает.
const VERIFY_STALE_MS: i64 = 8 * DAY_MS;
/// `selfsync-backup.timer` — раз в сутки.
const BACKUP_STALE_MS: i64 = 2 * DAY_MS;

pub fn doctor(config: &Config, opts: &Options) -> Report {
    let mut r = Report::default();
    if !data_dir(config, &mut r) {
        return r;
    }
    disk_space(&config.data_dir, &mut r);
    let vaults = databases(config, &mut r);
    public_url(config, &mut r);
    if opts.external {
        external(config, &mut r);
    }
    // Система под systemd: так проверяет и sd_booted().
    let systemd = opts.systemd && opts.root.join("run/systemd/system").is_dir();
    if systemd {
        timers(&opts.root, &mut r);
    }
    backups(config, opts, systemd, &mut r);
    for name in &vaults {
        vault(config, name, opts.now_ms, &mut r);
    }
    server_quota(config, &vaults, &mut r);
    r
}

fn data_dir(config: &Config, r: &mut Report) -> bool {
    let what = "каталог данных";
    let dir = &config.data_dir;
    let md = match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => m,
        Ok(_) => {
            r.add(Level::Fail, what, format!("{} — не каталог", dir.display()));
            return false;
        }
        Err(e) => {
            r.add(Level::Fail, what, format!("{}: {e}", dir.display()));
            return false;
        }
    };
    let probe = dir.join(format!(".doctor-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(e) => {
            r.add(
                Level::Fail,
                what,
                format!(
                    "{}: запись невозможна ({e}) — сервер не сможет сохранять",
                    dir.display()
                ),
            );
            return false;
        }
    }
    if md.permissions().mode() & 0o077 != 0 {
        r.add(
            Level::Warn,
            what,
            format!(
                "{} доступен другим пользователям системы (права {:o}) — там токены и \
                 заметки; chmod 700",
                dir.display(),
                md.permissions().mode() & 0o777
            ),
        );
    } else {
        r.add(Level::Ok, what, dir.display().to_string());
    }
    true
}

/// Свободно на файловой системе каталога: (свободно, всего).
fn free_space(dir: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: путь — валидная C-строка, st — выделенная структура нужного типа.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_bavail as u64 * frsize, st.f_blocks as u64 * frsize))
}

fn disk_space(dir: &Path, r: &mut Report) {
    let what = "место на диске";
    let Some((free, total)) = free_space(dir) else {
        r.add(Level::Skip, what, "не удалось узнать");
        return;
    };
    let text = format!("свободно {} из {}", fmt_bytes(free), fmt_bytes(total));
    if free < GIB / 10 {
        r.add(
            Level::Fail,
            what,
            format!("{text} — сервер вот-вот перестанет принимать файлы"),
        );
    } else if free < GIB || free.saturating_mul(20) < total {
        r.add(Level::Warn, what, format!("{text} — мало"));
    } else {
        r.add(Level::Ok, what, text);
    }
}

fn databases(config: &Config, r: &mut Report) -> Vec<String> {
    let what = "базы";
    if let Err(e) = crate::cmd::server_conn(config) {
        r.add(Level::Fail, what, format!("server.db не открывается: {e}"));
        return Vec::new();
    }
    let names = list_vault_dirs(config);
    let mut ok = Vec::new();
    for n in names {
        match open_vault(config, &n, 1) {
            Ok(_) => ok.push(n),
            Err(e) => r.add(
                Level::Fail,
                what,
                format!("vault «{n}» не открывается: {e}"),
            ),
        }
    }
    r.add(
        Level::Ok,
        what,
        format!("server.db и vault'ов: {}", ok.len()),
    );
    ok
}

/// Хост и порт из URL (`https://notes.example.com:8443/x` → `notes.example.com:8443`).
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    rest.split('/').next().unwrap_or(rest)
}

fn is_local(host: &str) -> bool {
    let h = host.rsplit_once(':').map_or(host, |x| x.0);
    h == "localhost" || h.starts_with("127.") || h == "[::1]"
}

fn public_url(config: &Config, r: &mut Report) {
    let what = "внешний адрес";
    let Some(url) = &config.public_url else {
        r.add(
            Level::Warn,
            what,
            "SELFSYNC_PUBLIC_URL не задан: ссылки подключения строятся из заголовков \
             запроса и за прокси могут указать на внутренний адрес",
        );
        return;
    };
    let host = host_of(url);
    if url.starts_with("http://") && !is_local(host) {
        r.add(
            Level::Warn,
            what,
            format!(
                "{url} — без TLS: токены и (без шифрования) заметки идут открыто, а \
                 мобильный Obsidian может отказаться соединяться"
            ),
        );
    } else {
        r.add(Level::Ok, what, url.clone());
    }
    // https на стандартном порту — почти всегда обратный прокси.
    let proxied = url.starts_with("https://") && !host.contains(':');
    if proxied && !config.trusted_proxy {
        r.add(
            Level::Warn,
            "прокси",
            "похоже, сервер за обратным прокси, а SELFSYNC_TRUSTED_PROXY выключен: лимит \
             неудачных попыток считает всех клиентов одним адресом прокси",
        );
    }
}

fn has_command(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn external(config: &Config, r: &mut Report) {
    let what = "снаружи";
    let Some(url) = &config.public_url else {
        return;
    };
    if !has_command("curl") {
        r.add(
            Level::Skip,
            what,
            "curl не найден — проверьте с устройства: selfsync-cli check или «Проверить \
             соединение» в плагине",
        );
        return;
    }
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "15", "-o", "-", "-w", "\n%{http_code}"])
        .arg(format!("{url}/v1/health"))
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            r.add(Level::Skip, what, format!("curl: {e}"));
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    match out.status.code() {
        Some(0) if stdout.contains("\"status\":\"ok\"") => {
            r.add(Level::Ok, what, format!("{url} отвечает"));
        }
        Some(0) => r.add(
            Level::Fail,
            what,
            format!(
                "{url}/v1/health отвечает не как selfsync (HTTP {}) — проверьте путь в прокси",
                stdout.lines().last().unwrap_or("?")
            ),
        ),
        // 60 — сертификат не принят, 51 — не на то имя, 35 — TLS не договорился.
        Some(60 | 51 | 35) => r.add(
            Level::Fail,
            what,
            format!("сертификат {url} не принят: {}", stderr.trim()),
        ),
        _ => r.add(
            Level::Fail,
            what,
            format!("{url} недоступен отсюда: {}", stderr.trim()),
        ),
    }
}

/// Состояние таймера по файлам, а не через `systemctl`: от пользователя сервиса
/// (`selfsync-admin doctor`) шины systemd нет, и `systemctl` только ошибается.
fn timer_state(root: &Path, unit: &str) -> &'static str {
    let wants = root
        .join("etc/systemd/system/timers.target.wants")
        .join(unit);
    if std::fs::symlink_metadata(&wants).is_ok() {
        return "enabled";
    }
    let installed = [
        "etc/systemd/system",
        "usr/lib/systemd/system",
        "lib/systemd/system",
    ]
    .iter()
    .any(|d| root.join(d).join(unit).exists());
    if installed { "disabled" } else { "missing" }
}

fn timers(root: &Path, r: &mut Report) {
    for (unit, why) in [
        (
            "selfsync-verify.timer",
            "порча хранилища останется незамеченной",
        ),
        ("selfsync-backup.timer", "бэкапов не будет"),
        (
            "selfsync-sweep.timer",
            "удалённое не стирается после срока хранения",
        ),
    ] {
        match timer_state(root, unit) {
            "enabled" => r.add(Level::Ok, unit, "включён"),
            "disabled" => r.add(
                Level::Warn,
                unit,
                format!("не включён — {why}; systemctl enable --now {unit}"),
            ),
            _ => r.add(
                Level::Warn,
                unit,
                format!(
                    "не установлен — {why}; юниты — в deploy/systemd репозитория, затем \
                     systemctl enable --now {unit}"
                ),
            ),
        }
    }
}

fn backups(config: &Config, opts: &Options, systemd: bool, r: &mut Report) {
    let what = "бэкапы";
    let dir = &opts.backup_dir;
    let Ok(md) = std::fs::metadata(dir) else {
        // Без systemd бэкапы могут жить где угодно: не знаем, где искать.
        let level = if systemd { Level::Warn } else { Level::Skip };
        r.add(level, what, format!("{} не найден", dir.display()));
        return;
    };
    let data_dev = std::fs::metadata(&config.data_dir).map(|m| m.dev()).ok();
    if data_dev == Some(md.dev()) {
        r.add(
            Level::Warn,
            what,
            format!(
                "{} на том же диске, что и данные — отказ диска унесёт и их; смонтируйте \
                 туда другой диск",
                dir.display()
            ),
        );
    }
    let newest = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name().to_str().is_some_and(|n| !n.starts_with('.')) && e.path().is_dir()
        })
        .filter_map(|e| e.metadata().ok())
        .map(|m| crate::blobs::mtime_ms(&m))
        .max();
    match newest {
        None => r.add(Level::Warn, what, format!("в {} пусто", dir.display())),
        Some(at) if opts.now_ms - at > BACKUP_STALE_MS => r.add(
            Level::Warn,
            what,
            format!("последний — {} дн. назад", (opts.now_ms - at) / DAY_MS),
        ),
        Some(_) => r.add(Level::Ok, what, format!("свежий в {}", dir.display())),
    }
}

fn vault(config: &Config, name: &str, now: i64, r: &mut Report) {
    let what = format!("vault «{name}»");
    let Ok(v) = open_vault(config, name, 1) else {
        return;
    };
    let Ok(c) = v.pool.get() else { return };
    let m = |k: &str| meta_int(&c, k).ok().flatten();
    match m("verify_at") {
        None => r.add(
            Level::Warn,
            &what,
            "selfsync verify ещё не запускался — включите selfsync-verify.timer",
        ),
        Some(_)
            if m("verify_db_errors").unwrap_or(0) > 0
                || m("verify_key_problem").unwrap_or(0) > 0 =>
        {
            r.add(
                Level::Fail,
                &what,
                "последний verify нашёл ошибки базы или записи ключа — selfsync verify",
            );
        }
        Some(at) if now - at > VERIFY_STALE_MS => r.add(
            Level::Warn,
            &what,
            format!("последний verify — {} дн. назад", (now - at) / DAY_MS),
        ),
        Some(_) => r.add(Level::Ok, &what, "verify свежий"),
    }
    let wanted = crate::db::vault::vault_state(&c)
        .map(|s| s.blobs_wanted)
        .unwrap_or(0);
    if wanted > 0 {
        r.add(
            Level::Warn,
            &what,
            format!("повреждено блобов: {wanted}, ждут копий с устройств"),
        );
    }
    let l = crate::quota::limits(&c, &config.quota.defaults).unwrap_or_default();
    let used = crate::quota::usage(&c).unwrap_or(0);
    if l.hard > 0 && used >= l.hard {
        r.add(
            Level::Fail,
            &what,
            format!(
                "место закончилось: {} из {} — новые файлы не принимаются",
                fmt_bytes(used),
                fmt_bytes(l.hard)
            ),
        );
    } else if l.soft > 0 && used >= l.soft {
        r.add(
            Level::Warn,
            &what,
            format!(
                "занято {} — больше порога {}",
                fmt_bytes(used),
                fmt_bytes(l.soft)
            ),
        );
    }
    if let Ok(sc) = crate::cmd::server_conn(config)
        && let Ok(n) = server::count_devices(&sc, name)
        && l.devices > 0
        && n >= l.devices
    {
        r.add(
            Level::Warn,
            &what,
            format!("устройств {n} из {} — новых не подключить", l.devices),
        );
    }
}

fn server_quota(config: &Config, vaults: &[String], r: &mut Report) {
    let hard = config.quota.server_hard;
    if hard == 0 {
        return;
    }
    let used: u64 = vaults
        .iter()
        .filter_map(|n| open_vault(config, n, 1).ok())
        .filter_map(|v| v.pool.get().ok().and_then(|c| crate::quota::usage(&c).ok()))
        .sum();
    let text = format!("занято {} из {}", fmt_bytes(used), fmt_bytes(hard));
    let what = "квота сервера";
    if used >= hard {
        r.add(
            Level::Fail,
            what,
            format!("{text} — новые файлы не принимаются"),
        );
    } else if used.saturating_mul(10) >= hard.saturating_mul(9) {
        r.add(Level::Warn, what, format!("{text} — почти всё"));
    } else {
        r.add(Level::Ok, what, text);
    }
}

fn fmt_bytes(n: u64) -> String {
    crate::cmd::fmt_bytes(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_states() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc/systemd/system");
        std::fs::create_dir_all(etc.join("timers.target.wants")).unwrap();
        std::fs::write(etc.join("selfsync-verify.timer"), "").unwrap();
        std::fs::write(etc.join("selfsync-sweep.timer"), "").unwrap();
        std::os::unix::fs::symlink(
            "../selfsync-sweep.timer",
            etc.join("timers.target.wants/selfsync-sweep.timer"),
        )
        .unwrap();
        assert_eq!(timer_state(root.path(), "selfsync-sweep.timer"), "enabled");
        assert_eq!(
            timer_state(root.path(), "selfsync-verify.timer"),
            "disabled"
        );
        assert_eq!(timer_state(root.path(), "selfsync-backup.timer"), "missing");
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://notes.example.com/x"), "notes.example.com");
        assert_eq!(host_of("http://127.0.0.1:8085"), "127.0.0.1:8085");
        assert!(is_local("127.0.0.1:8085"));
        assert!(is_local("localhost"));
        assert!(!is_local("87.249.49.6:18087"));
    }
}
