//! Служебные подкоманды. Работают напрямую с БД и файлами (без HTTP), безопасно
//! параллельно с работающим сервером: те же транзакции и блокировки.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use notesync_core::blob::{BlobHeader, DEFAULT_CHUNK_SIZE};
use notesync_core::hash::{Hash, Hasher};
use notesync_core::path::VaultPath;
use notesync_proto::v1 as pb;

use crate::blobs::BlobStore;
use crate::config::Config;
use crate::db::{self, now_ms, server, vault};
use crate::gc::{self, GcOptions};
use crate::state::{list_vault_dirs, open_vault, vault_dir};

fn server_conn(config: &Config) -> anyhow::Result<rusqlite::Connection> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("не создаётся каталог данных {}", config.data_dir.display()))?;
    let mut c = db::open(&config.data_dir.join("server.db"))?;
    server::migrate_server(&mut c)?;
    Ok(c)
}

fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_owned();
    }
    let t = std::time::UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0));
    humantime::format_rfc3339_seconds(t).to_string()
}

fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn migrate(config: &Config, only: Option<&str>) -> anyhow::Result<()> {
    server_conn(config)?;
    println!("server.db: схема актуальна");
    let names = match only {
        Some(v) => vec![v.to_owned()],
        None => list_vault_dirs(config),
    };
    for name in names {
        open_vault(config, &name, 1)?;
        println!("vault {name}: схема актуальна");
    }
    Ok(())
}

pub fn vault_list(config: &Config) -> anyhow::Result<()> {
    let c = server_conn(config)?;
    let mut names = list_vault_dirs(config);
    for n in server::vault_names(&c)? {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names.sort();
    if names.is_empty() {
        println!("vault'ов нет. Создайте первый: notesync token add --vault notes --name laptop");
    }
    for name in names {
        let v = open_vault(config, &name, 1)?;
        let vc = v.pool.get()?;
        let s = vault::stats(&vc)?;
        let (bytes, _) = v.blobs.total_size()?;
        let devices = server::count_devices(&c, &name)?;
        println!(
            "{name}\tseq={}\tфайлов={}\tустройств={devices}\tна диске={}",
            s.seq,
            s.files,
            fmt_bytes(bytes)
        );
    }
    Ok(())
}

pub fn token_add(config: &Config, vault_name: &str, name: &str) -> anyhow::Result<()> {
    let c = server_conn(config)?;
    let (id, token) = server::add_device(&c, vault_name, name)?;
    open_vault(config, vault_name, 1)?;
    println!("Устройство #{id} «{name}» добавлено в vault «{vault_name}».");
    println!("Токен (показывается один раз):");
    println!("{token}");
    Ok(())
}

pub fn token_list(config: &Config, vault_name: Option<&str>) -> anyhow::Result<()> {
    let c = server_conn(config)?;
    let rows = server::list_devices(&c, vault_name)?;
    if rows.is_empty() {
        println!("устройств нет");
    }
    for d in rows {
        println!(
            "#{}\t{}\t{}\tсоздано {}\tбыл {}\t{}",
            d.id,
            d.vault,
            d.name,
            fmt_time(d.created_at),
            fmt_time(d.last_seen),
            if d.revoked {
                "ОТОЗВАН"
            } else {
                "активен"
            }
        );
    }
    Ok(())
}

pub fn token_revoke(config: &Config, vault_name: Option<&str>, name: &str) -> anyhow::Result<()> {
    let c = server_conn(config)?;
    let n = server::revoke_by_name(&c, vault_name, name)?;
    if n == 0 {
        bail!("активных устройств с именем «{name}» не найдено");
    }
    println!("отозвано устройств: {n}");
    Ok(())
}

/// QR-код в терминал (полублоки Unicode, по две строки модулей на строку текста).
pub fn render_qr(data: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let w = code.width();
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 || x >= w as isize || y >= w as isize {
            return false;
        }
        colors[(y as usize) * w + x as usize] == qrcode::Color::Dark
    };
    let quiet = 2isize;
    let mut out = String::new();
    let mut y = -quiet;
    while y < w as isize + quiet {
        for x in -quiet..w as isize + quiet {
            let top = dark(x, y);
            let bottom = dark(x, y + 1);
            // Инверсия: светлые модули рисуются, тёмные — пробел (тёмный фон терминала).
            out.push(match (top, bottom) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            });
        }
        out.push('\n');
        y += 2;
    }
    Ok(out)
}

pub fn link(
    config: &Config,
    vault_name: &str,
    url: Option<&str>,
    name: &str,
) -> anyhow::Result<()> {
    let base = url
        .map(str::to_owned)
        .or_else(|| config.public_url.clone())
        .context("нужен внешний адрес: --url https://notes.example.com или NOTESYNC_PUBLIC_URL")?;
    let base = base.trim_end_matches('/');
    let c = server_conn(config)?;
    open_vault(config, vault_name, 1)?;
    let (code, expires) = server::create_join_code(&c, vault_name, name, None)?;
    let link = format!("{base}/join/{code}");
    println!(
        "Ссылка подключения «{name}» к vault'у «{vault_name}» (одноразовая, до {}):",
        fmt_time(expires)
    );
    println!("{link}\n");
    print!("{}", render_qr(&link)?);
    println!("\nКод для ручного ввода: {code}");
    Ok(())
}

/// Пишет файл как открытый блоб NSB потоком: заголовок + содержимое.
fn put_plain_file(store: &BlobStore, path: &Path) -> anyhow::Result<(Hash, u64)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let header = BlobHeader {
        flags: 0,
        chunk_size: DEFAULT_CHUNK_SIZE,
        plaintext_len: len,
    }
    .to_bytes();
    let mut tmp_name = [0u8; 12];
    getrandom::fill(&mut tmp_name).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let tmp = store.uploads_dir().join(format!(
        "tmp-import-{}",
        tmp_name
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    let mut out = std::fs::File::create(&tmp)?;
    let mut h = Hasher::new();
    h.update(&header);
    out.write_all(&header)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        total += n as u64;
    }
    drop(out);
    if total != len {
        let _ = std::fs::remove_file(&tmp);
        bail!("файл изменился во время импорта: {}", path.display());
    }
    let hash = h.finish();
    store.finalize(&tmp, &hash)?;
    Ok((hash, header.len() as u64 + len))
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub files: u64,
    pub unchanged: u64,
    pub folders: u64,
    pub skipped: Vec<(String, String)>,
}

/// Загрузка существующей папки открытым текстом.
pub fn import(
    config: &Config,
    vault_name: &str,
    from: &Path,
    max_blob: u64,
) -> anyhow::Result<ImportReport> {
    anyhow::ensure!(from.is_dir(), "{} — не каталог", from.display());
    let v = open_vault(config, vault_name, 1)?;
    let mut c = v.pool.get()?;
    if vault::meta_blob(&c, "vault_key")?.is_some() {
        bail!(
            "vault зашифрован: импорт открытым текстом запрещён. Импортируйте до включения шифрования."
        );
    }
    let mut report = ImportReport::default();
    let mut ops: Vec<pb::Op> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![from.to_path_buf()];
    let flush = |c: &mut rusqlite::Connection,
                 ops: &mut Vec<pb::Op>,
                 report: &mut ImportReport|
     -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let store = v.blobs.clone();
        let exists = move |h: &Hash| store.exists(h);
        let ctx = vault::OpsCtx {
            device: 0,
            now: now_ms(),
            max_blob_size: max_blob,
            blob_exists: &exists,
        };
        let out = vault::apply_ops(c, &ctx, ops)?;
        for (op, r) in ops.iter().zip(out.results) {
            let is_dir = matches!(op.kind, Some(pb::op::Kind::Mkdir(_)));
            match r.result {
                Some(pb::op_result::Result::Applied(a)) if a.noop => {
                    if !is_dir {
                        report.unchanged += 1;
                    }
                }
                Some(pb::op_result::Result::Applied(_)) => {
                    if is_dir {
                        report.folders += 1;
                    } else {
                        report.files += 1;
                    }
                }
                other => report.skipped.push(("?".into(), format!("{other:?}"))),
            }
        }
        ops.clear();
        Ok(())
    };
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(from)?.to_string_lossy().replace('\\', "/");
            let ft = e.file_type()?;
            if ft.is_symlink() {
                report.skipped.push((rel, "символическая ссылка".into()));
                continue;
            }
            let vp = match VaultPath::normalize(&rel) {
                Ok(vp) => vp,
                Err(err) => {
                    report.skipped.push((rel, err.to_string()));
                    continue;
                }
            };
            if ft.is_dir() {
                ops.push(pb::Op {
                    kind: Some(pb::op::Kind::Mkdir(pb::Mkdir {
                        path: Some(vp.to_proto()),
                    })),
                });
                stack.push(p);
            } else if ft.is_file() {
                let md = e.metadata()?;
                if md.len() > max_blob {
                    report.skipped.push((rel, "больше лимита размера".into()));
                    continue;
                }
                let (hash, size) = put_plain_file(&v.blobs, &p)?;
                let key = notesync_core::path::canonical_encode(&vp.to_proto());
                let base_rev = vault::get_file(&c, &key)?.map_or(0, |r| r.rev);
                ops.push(pb::Op {
                    kind: Some(pb::op::Kind::Put(pb::Put {
                        path: Some(vp.to_proto()),
                        base_rev,
                        hash: hash.to_vec(),
                        size,
                        mtime: crate::blobs::mtime_ms(&md),
                    })),
                });
            }
            if ops.len() >= 500 {
                flush(&mut c, &mut ops, &mut report)?;
            }
        }
    }
    flush(&mut c, &mut ops, &mut report)?;
    Ok(report)
}

pub fn gc(
    config: &Config,
    only: Option<&str>,
    opts: GcOptions,
    execute: bool,
) -> anyhow::Result<()> {
    let names = match only {
        Some(v) => vec![v.to_owned()],
        None => list_vault_dirs(config),
    };
    let now = now_ms();
    for name in names {
        let v = open_vault(config, &name, 1)?;
        let mut c = v.pool.get()?;
        let plan = gc::plan(&c, &v.blobs, opts, now)?;
        println!("vault {name}:");
        println!("  ревизий истории к удалению:   {}", plan.revisions.len());
        println!("  tombstone'ов (с историей):    {}", plan.tombstones.len());
        println!("  блобов без ссылок:            {}", plan.blobs.len());
        println!(
            "  брошенных загрузок:           {}",
            plan.uploads.len() + plan.stray_files.len()
        );
        println!(
            "  освободится:                  {}",
            fmt_bytes(plan.bytes())
        );
        if plan.is_empty() {
            continue;
        }
        if execute {
            let r = gc::execute(&mut c, &v.blobs, &plan)?;
            println!(
                "  выполнено: ревизий {}, tombstone'ов {}, блобов {}, загрузок {}",
                r.revisions, r.tombstones, r.blobs, r.uploads
            );
        }
    }
    if !execute {
        println!("\nЭто только план. Чтобы удалить, запустите с --yes.");
    }
    Ok(())
}

pub fn sweep(config: &Config) -> anyhow::Result<()> {
    for (name, r) in crate::sweep::sweep_all(config)? {
        println!("vault {name}: стёрто путей {}, блобов {}", r.paths, r.blobs);
    }
    Ok(())
}

fn link_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        return Ok(());
    }
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(src, dst).map(|_| ()),
    }
}

/// Консистентная копия: `VACUUM INTO` для каждой БД, блобы — жёсткими ссылками
/// (или копией на другой ФС). Блобы неизменяемы, так что снимок БД, сделанный
/// раньше копирования блобов, ссылается только на существующие блобы
/// (не запускайте `gc --yes` одновременно с бэкапом).
pub fn backup(config: &Config, dest: &Path) -> anyhow::Result<()> {
    if dest.join("server.db").exists() {
        bail!(
            "{} уже содержит бэкап — укажите пустой каталог",
            dest.display()
        );
    }
    std::fs::create_dir_all(dest)?;
    let c = server_conn(config)?;
    let target = dest.join("server.db");
    c.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
    let mut count = 0u64;
    for name in list_vault_dirs(config) {
        let v = open_vault(config, &name, 1)?;
        let vdest = dest.join("vaults").join(&name);
        std::fs::create_dir_all(vdest.join("blobs"))?;
        std::fs::create_dir_all(vdest.join("uploads"))?;
        {
            let vc = v.pool.get()?;
            let t = vdest.join("meta.db");
            vc.execute("VACUUM INTO ?1", [t.to_string_lossy().as_ref()])?;
        }
        for (h, _, _) in v.blobs.list()? {
            let hex = h.to_hex();
            let d = vdest.join("blobs").join(&hex[..2]);
            std::fs::create_dir_all(&d)?;
            link_or_copy(&v.blobs.path_of(&h), &d.join(&hex))?;
            count += 1;
        }
        println!(
            "vault {name}: скопирован ({})",
            vault_dir(config, &name).display()
        );
    }
    println!("бэкап готов: {} (блобов: {count})", dest.display());
    Ok(())
}

/// Проверка живости для `HEALTHCHECK` в distroless: GET /v1/health, код 0/1.
pub fn healthcheck(url: &str) -> bool {
    healthcheck_inner(url).unwrap_or(false)
}

fn healthcheck_inner(url: &str) -> anyhow::Result<bool> {
    let timeout = Duration::from_secs(3);
    let request = |host: &str, path: &str| {
        format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n")
    };
    let mut resp = Vec::new();
    if let Some(rest) = url.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            let mut s = std::os::unix::net::UnixStream::connect(rest)?;
            s.set_read_timeout(Some(timeout))?;
            s.write_all(request("localhost", "/v1/health").as_bytes())?;
            s.read_to_end(&mut resp)?;
        }
        #[cfg(not(unix))]
        {
            let _ = rest;
            bail!("unix-сокеты не поддерживаются");
        }
    } else {
        let rest = url
            .strip_prefix("http://")
            .context("поддерживается только http:// или unix:")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/v1/health"),
        };
        use std::net::ToSocketAddrs;
        let addr = hostport
            .to_socket_addrs()?
            .next()
            .context("адрес не разрешается")?;
        let mut s = std::net::TcpStream::connect_timeout(&addr, timeout)?;
        s.set_read_timeout(Some(timeout))?;
        s.write_all(request(hostport, path).as_bytes())?;
        s.read_to_end(&mut resp)?;
    }
    let text = String::from_utf8_lossy(&resp);
    let status_ok = text
        .lines()
        .next()
        .is_some_and(|l| l.split_whitespace().nth(1) == Some("200"));
    Ok(status_ok && text.contains("\"ok\""))
}
