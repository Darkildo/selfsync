//! Исполнитель действий ядра на обычной ФС и HTTP (reqwest). Контракт — как у
//! исполнителя плагина: условия записи, Precondition/NotFound, настоящие имена.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use selfsync_core::engine::{Action, Expect, FileMeta, HttpRequest, IoResult};
use selfsync_core::path::MAX_SEGMENT_BYTES;
use selfsync_core::tempname::{self, TEMP_SUFFIX};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::config::{STATE_DIR, TRASH_DIR};

pub struct Exec {
    root: PathBuf,
    state: PathBuf,
    server: String,
    token: Option<String>,
    http: reqwest::Client,
    /// `.selfsync/lock` под flock: индекс папки держит один процесс.
    _lock: Option<std::fs::File>,
    /// Снимки индекса пишутся по одному (общий временный файл) и не старее
    /// записанного: id действия растёт в порядке выдачи снимков ядром.
    index_saved: tokio::sync::Mutex<u64>,
}

/// Файл-замок папки в `.selfsync/`.
pub const LOCK_FILE: &str = "lock";

/// Эксклюзивный замок папки. Ждёт до двух секунд: только что завершившийся запуск
/// мог ещё не отпустить его (задачи ввода-вывода дочищаются после выхода).
#[cfg(unix)]
fn lock_folder(state: &Path) -> anyhow::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(state)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join(LOCK_FILE))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: flock(2) над открытым дескриптором файла, которым мы владеем.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(f));
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "папка синхронизируется другим процессом ({}). Остановите его, например \
                 `systemctl --user stop selfsync-cli@<папка>`, и повторите",
                state.join(LOCK_FILE).display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(unix))]
fn lock_folder(_state: &Path) -> anyhow::Result<Option<std::fs::File>> {
    Ok(None)
}

/// Ошибка со всеми причинами: у reqwest сама ошибка — «error sending request», а
/// «connection refused» или ошибка сертификата — в `source()`.
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

fn failed(e: impl std::fmt::Display) -> IoResult {
    IoResult::Failed {
        message: e.to_string(),
    }
}

fn not_found(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

impl Exec {
    pub fn new(root: &Path, server: &str, token: Option<String>) -> anyhow::Result<Exec> {
        // Провайдер криптографии rustls ставится один раз на процесс.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        let state = root.join(STATE_DIR);
        let lock = lock_folder(&state)?;
        Ok(Exec {
            root: root.to_path_buf(),
            state,
            server: server.trim_end_matches('/').to_owned(),
            token,
            http,
            _lock: lock,
            index_saved: tokio::sync::Mutex::new(0),
        })
    }

    pub fn index_path(&self) -> PathBuf {
        self.state.join("index.bin")
    }

    pub fn key_path(&self) -> PathBuf {
        self.state.join("key")
    }

    fn abs(&self, p: &str) -> PathBuf {
        if p.is_empty() {
            self.root.clone()
        } else {
            self.root.join(p)
        }
    }

    fn temp(&self, name: &str) -> PathBuf {
        self.state.join("tmp").join(name)
    }

    fn cache(&self, key: &str) -> PathBuf {
        self.state.join("cache").join(key)
    }

    /// Выполнить действие с ответом. Действия без ответа сюда не попадают.
    pub async fn perform(&self, a: Action) -> IoResult {
        match self.dispatch(a).await {
            Ok(r) => r,
            Err(e) => failed(e),
        }
    }

    async fn dispatch(&self, a: Action) -> std::io::Result<IoResult> {
        Ok(match a {
            Action::Http { req, .. } => self.http(req).await,
            Action::List { .. } => IoResult::Listing {
                files: self.list().await?,
            },
            Action::Stat { path, .. } => IoResult::Stat {
                meta: self.stat(&path).await?,
            },
            Action::Read {
                path, offset, len, ..
            } => {
                // Через ссылку — чужой файл, не из vault'а: его здесь нет.
                if self.link_on(&path).await?.is_some() {
                    return Ok(IoResult::NotFound);
                }
                match read_range(&self.abs(&path), offset, len).await? {
                    Some(data) => IoResult::Data { data },
                    None => IoResult::NotFound,
                }
            }
            Action::Write {
                path, data, expect, ..
            } => {
                // Не выполнено уже сейчас — не писать зря.
                if !self.holds(&path, &expect).await?
                    || self.stat(&path).await?.is_some_and(|m| m.dir)
                {
                    return Ok(IoResult::Precondition);
                }
                self.inside(&path).await?;
                let target = self.abs(&path);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                // Рядом с файлом (rename в пределах каталога атомарен); имени до 255 байт
                // не хватило бы места под суффикс — база тогда укорочена.
                let tmp = PathBuf::from(format!(
                    "{}{TEMP_SUFFIX}",
                    self.abs(&tempname::temp_base(&path)).display()
                ));
                write_durable(&tmp, &data).await?;
                self.place(&tmp, &path, &expect, true).await?
            }
            Action::WriteTemp {
                temp, offset, data, ..
            } => {
                let f = self.temp(&temp);
                if offset == 0 {
                    tokio::fs::create_dir_all(self.state.join("tmp")).await?;
                    write_durable(&f, &data).await?;
                } else {
                    let mut fh = tokio::fs::OpenOptions::new().write(true).open(&f).await?;
                    let size = fh.metadata().await?.len();
                    if size < offset {
                        return Ok(failed(format!(
                            "разрыв во временном файле {temp}: {size} < {offset}"
                        )));
                    }
                    fh.set_len(offset).await?;
                    fh.seek(std::io::SeekFrom::Start(offset)).await?;
                    fh.write_all(&data).await?;
                    fh.sync_all().await?;
                }
                IoResult::Done
            }
            Action::ReadTemp {
                temp, offset, len, ..
            } => match read_range(&self.temp(&temp), offset, Some(len)).await? {
                Some(data) => IoResult::Data { data },
                None => IoResult::NotFound,
            },
            Action::CommitTemp {
                temp, path, expect, ..
            } => {
                if !self.holds(&path, &expect).await?
                    || self.stat(&path).await?.is_some_and(|m| m.dir)
                {
                    return Ok(IoResult::Precondition);
                }
                self.inside(&path).await?;
                let target = self.abs(&path);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                self.place(&self.temp(&temp), &path, &expect, false).await?
            }
            Action::DeleteTemp { temp, .. } => {
                match tokio::fs::remove_file(self.temp(&temp)).await {
                    Err(e) if !not_found(&e) => return Err(e),
                    _ => {}
                }
                IoResult::Done
            }
            Action::Delete { path, expect, .. } => {
                let Some(cur) = self.stat(&path).await? else {
                    return Ok(IoResult::NotFound);
                };
                if cur.dir || !matches(&cur, &expect) {
                    return Ok(IoResult::Precondition);
                }
                self.inside(&cur.path).await?;
                tokio::fs::remove_file(self.abs(&cur.path)).await?;
                IoResult::Done
            }
            Action::Trash { path, expect, .. } => {
                let Some(cur) = self.stat(&path).await? else {
                    return Ok(IoResult::NotFound);
                };
                if cur.dir || !matches(&cur, &expect) {
                    return Ok(IoResult::Precondition);
                }
                self.inside(&cur.path).await?;
                self.trash(&cur.path).await?;
                IoResult::Done
            }
            Action::Rename { from, to, .. } => {
                let Some(src) = self.stat(&from).await? else {
                    return Ok(IoResult::NotFound);
                };
                if self.stat(&to).await?.is_some_and(|d| d.path != src.path) {
                    return Ok(IoResult::Precondition);
                }
                self.inside(&src.path).await?;
                self.inside(&to).await?;
                let target = self.abs(&to);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::rename(self.abs(&src.path), &target).await?;
                IoResult::Stat {
                    meta: self.stat(&to).await?,
                }
            }
            Action::Mkdir { path, .. } => {
                self.inside(&path).await?;
                tokio::fs::create_dir_all(self.abs(&path)).await?;
                IoResult::Done
            }
            Action::Rmdir { path, .. } => {
                self.inside(&path).await?;
                match tokio::fs::remove_dir(self.abs(&path)).await {
                    Ok(()) => IoResult::Done,
                    Err(e) if not_found(&e) => IoResult::NotFound,
                    Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                        IoResult::Precondition
                    }
                    Err(e) => return Err(e),
                }
            }
            Action::SaveIndex { id, data } => {
                // Задача синка и, например, решение конфликта сохраняют индекс
                // параллельно: без замка второй rename не находит временный файл.
                let mut saved = self.index_saved.lock().await;
                if id < *saved {
                    return Ok(IoResult::Done);
                }
                *saved = id;
                tokio::fs::create_dir_all(&self.state).await?;
                let tmp = self.state.join(format!("index.bin{TEMP_SUFFIX}"));
                write_durable(&tmp, &data).await?;
                tokio::fs::rename(&tmp, self.index_path()).await?;
                IoResult::Done
            }
            Action::CacheRead { key, .. } => match tokio::fs::read(self.cache(&key)).await {
                Ok(data) => IoResult::Data { data },
                Err(e) if not_found(&e) => IoResult::NotFound,
                Err(e) => return Err(e),
            },
            Action::CacheWrite { key, data, .. } => {
                tokio::fs::create_dir_all(self.state.join("cache")).await?;
                write_durable(&self.cache(&key), &data).await?;
                IoResult::Done
            }
            Action::CacheDelete { key, .. } => {
                match tokio::fs::remove_file(self.cache(&key)).await {
                    Err(e) if !not_found(&e) => return Err(e),
                    _ => {}
                }
                IoResult::Done
            }
            other => failed(format!("действие без ответа: {other:?}")),
        })
    }

    async fn http(&self, req: HttpRequest) -> IoResult {
        let method = match reqwest::Method::from_bytes(req.method.as_bytes()) {
            Ok(m) => m,
            Err(e) => return failed(e),
        };
        let mut b = self
            .http
            .request(method, format!("{}{}", self.server, req.path))
            .timeout(Duration::from_millis(req.timeout_ms.max(1000)));
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        if req.auth
            && let Some(t) = &self.token
        {
            b = b.bearer_auth(t);
        }
        if !req.body.is_empty() {
            b = b.body(req.body);
        }
        match b.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                    .collect();
                match resp.bytes().await {
                    Ok(body) => IoResult::Http {
                        status,
                        headers,
                        body: body.to_vec(),
                    },
                    Err(e) => failed(chain(&e)),
                }
            }
            Err(e) => failed(chain(&e)),
        }
    }

    async fn list(&self) -> std::io::Result<Vec<FileMeta>> {
        let mut out = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let mut rd = tokio::fs::read_dir(self.abs(&rel)).await?;
            while let Some(e) = rd.next_entry().await? {
                let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                    continue; // не-UTF-8 имя: ядро его всё равно не примет
                };
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                let ft = e.file_type().await?;
                if ft.is_dir() {
                    out.push(FileMeta {
                        path: path.clone(),
                        size: 0,
                        mtime: 0,
                        dir: true,
                        link: false,
                        ctime: 0,
                    });
                    stack.push(path);
                } else if ft.is_file() {
                    let md = e.metadata().await?;
                    out.push(FileMeta {
                        path,
                        size: md.len(),
                        mtime: mtime_ms(&md),
                        dir: false,
                        link: false,
                        ctime: ctime_ms(&md),
                    });
                } else if ft.is_symlink() {
                    // Ссылка (на папку или файл) может вести за пределы vault'а: не
                    // синхронизируется, ядро исключает её вместе со всем под ней.
                    out.push(FileMeta {
                        path,
                        size: 0,
                        mtime: 0,
                        dir: false,
                        link: true,
                        ctime: 0,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Первая символическая ссылка на пути — сама запись или папка выше (корень
    /// vault'а не в счёт). Путь через ссылку уходит за пределы vault'а: такие пути не
    /// читаются и не меняются.
    async fn link_on(&self, path: &str) -> std::io::Result<Option<String>> {
        let mut cur = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(part);
            let md = match tokio::fs::symlink_metadata(self.abs(&cur)).await {
                Ok(m) => m,
                // Дальше ничего нет — и ссылок тоже.
                Err(e) if not_found(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            if md.file_type().is_symlink() {
                return Ok(Some(cur));
            }
            if !md.is_dir() {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Изменять можно только пути без символических ссылок.
    async fn inside(&self, path: &str) -> std::io::Result<()> {
        match self.link_on(path).await? {
            Some(link) => Err(std::io::Error::other(format!(
                "{path}: путь ведёт через символическую ссылку {link} — вне vault'а не пишем"
            ))),
            None => Ok(()),
        }
    }

    /// Метаданные с настоящим именем: на регистронезависимой ФС «Note.md» может
    /// оказаться «note.md» — ядро обязано это видеть.
    async fn stat(&self, path: &str) -> std::io::Result<Option<FileMeta>> {
        if self.link_on(path).await?.is_some() {
            return Ok(Some(FileMeta {
                path: path.to_owned(),
                size: 0,
                mtime: 0,
                dir: false,
                link: true,
                ctime: 0,
            }));
        }
        let md = match tokio::fs::symlink_metadata(self.abs(path)).await {
            Ok(m) => m,
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let real = if cfg!(target_os = "linux") || path.is_empty() {
            path.to_owned()
        } else {
            self.real_name(path)
                .await
                .unwrap_or_else(|| path.to_owned())
        };
        Ok(Some(if md.is_dir() {
            FileMeta {
                path: real,
                size: 0,
                mtime: 0,
                dir: true,
                link: false,
                ctime: 0,
            }
        } else {
            FileMeta {
                path: real,
                size: md.len(),
                mtime: mtime_ms(&md),
                dir: false,
                link: false,
                ctime: ctime_ms(&md),
            }
        }))
    }

    async fn real_name(&self, path: &str) -> Option<String> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut rd = tokio::fs::read_dir(self.abs(parent)).await.ok()?;
        let mut folded = None;
        let key = name.to_lowercase();
        while let Ok(Some(e)) = rd.next_entry().await {
            let n = e.file_name().to_str()?.to_owned();
            if n == name {
                return Some(path.to_owned());
            }
            if n.to_lowercase() == key {
                folded = Some(n);
            }
        }
        folded.map(|n| {
            if parent.is_empty() {
                n
            } else {
                format!("{parent}/{n}")
            }
        })
    }

    async fn holds(&self, path: &str, e: &Expect) -> std::io::Result<bool> {
        Ok(match e {
            Expect::Any => true,
            Expect::Absent => self.stat(path).await?.is_none(),
            Expect::Stat { .. } => self.stat(path).await?.is_some_and(|m| matches(&m, e)),
        })
    }

    /// Поставить готовый файл `src` на место `path`, если путь всё ещё удовлетворяет
    /// `expect` (как `place` в плагине, `plugin/src/io/node.ts`). Атомарного «заменить,
    /// если не изменился» у ФС нет, поэтому заменяемый файл не стирается: на него
    /// ставится жёсткая ссылка `X.aside.selfsync-tmp`, условие сверяется по ней, а
    /// после замены видно, не дописали ли в него за это время, — тогда это
    /// вытесненная правка (`IoResult::Displaced`). `own` — `src` наш и при отказе
    /// удаляется (временный файл скачивания — нет).
    async fn place(
        &self,
        src: &Path,
        path: &str,
        expect: &Expect,
        own: bool,
    ) -> std::io::Result<IoResult> {
        let dst = self.abs(path);
        let mut displaced = None;
        match expect {
            // hard_link не заменяет существующий файл: «создать, только если имени нет».
            Expect::Absent => match tokio::fs::hard_link(src, &dst).await {
                Ok(()) => tokio::fs::remove_file(src).await?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return refuse(src, own).await;
                }
                Err(e) if no_links(&e) => {
                    if tokio::fs::symlink_metadata(&dst).await.is_ok() {
                        return refuse(src, own).await;
                    }
                    tokio::fs::rename(src, &dst).await?;
                }
                Err(e) => return Err(e),
            },
            Expect::Stat { size, mtime } => {
                let aside = self.free_aside(path).await?;
                let aside_abs = self.abs(&aside);
                let linked = match tokio::fs::hard_link(&dst, &aside_abs).await {
                    Ok(()) => true,
                    Err(e) if not_found(&e) => return refuse(src, own).await,
                    // Без жёстких ссылок (FAT, часть сетевых ФС) прежний файл
                    // отодвигается переименованием — на миг его нет на месте.
                    Err(e) if no_links(&e) => match tokio::fs::rename(&dst, &aside_abs).await {
                        Ok(()) => false,
                        Err(e) if not_found(&e) => return refuse(src, own).await,
                        Err(e) => return Err(e),
                    },
                    Err(e) => return Err(e),
                };
                let before = tokio::fs::symlink_metadata(&aside_abs).await?;
                if !before.is_file() || before.len() != *size || mtime_ms(&before) != *mtime {
                    // Изменился ещё до того, как его поймали: остаётся на месте.
                    if linked {
                        tokio::fs::remove_file(&aside_abs).await?;
                    } else if tokio::fs::symlink_metadata(&dst).await.is_err() {
                        tokio::fs::rename(&aside_abs, &dst).await?;
                    }
                    return refuse(src, own).await;
                }
                if !linked && tokio::fs::symlink_metadata(&dst).await.is_ok() {
                    // Пока отодвигали, на месте сохранили новый файл: он остаётся.
                    let _ = tokio::fs::remove_file(&aside_abs).await;
                    return refuse(src, own).await;
                }
                tokio::fs::rename(src, &dst).await?;
                // ctime здесь не годится: его меняет и сама замена (у отодвинутого
                // файла становится меньше имён). mtime — с полной точностью ФС.
                match tokio::fs::symlink_metadata(&aside_abs).await {
                    Ok(after)
                        if after.len() != before.len()
                            || after.modified().ok() != before.modified().ok() =>
                    {
                        displaced = Some(aside);
                    }
                    _ => {
                        let _ = tokio::fs::remove_file(&aside_abs).await;
                    }
                }
            }
            Expect::Any => tokio::fs::rename(src, &dst).await?,
        }
        let meta = self.stat(path).await?;
        Ok(match (displaced, meta) {
            (Some(aside), Some(meta)) => IoResult::Displaced { meta, aside },
            (_, meta) => IoResult::Stat { meta },
        })
    }

    /// Свободное имя, под которое отодвигается заменяемый файл.
    async fn free_aside(&self, path: &str) -> std::io::Result<String> {
        for n in 1..100 {
            let name = tempname::aside(path, n);
            if tokio::fs::symlink_metadata(self.abs(&name)).await.is_err() {
                return Ok(name);
            }
        }
        Err(std::io::Error::other(format!(
            "{path}: нет свободного временного имени"
        )))
    }

    /// В локальную корзину `.trash/` (как делает Obsidian), не стирая.
    async fn trash(&self, path: &str) -> std::io::Result<()> {
        let dir = self.root.join(TRASH_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        // Путь целиком в имени файла: глубокий путь длиннее 255 байт укорачивается
        // (место — под номер «N » впереди).
        let base = tempname::fit_name(&path.replace('/', " - "), MAX_SEGMENT_BYTES - 4);
        let mut dest = dir.join(&base);
        let mut n = 1;
        while tokio::fs::try_exists(&dest).await? {
            n += 1;
            dest = dir.join(format!("{n} {base}"));
        }
        tokio::fs::rename(self.abs(path), dest).await
    }
}

fn matches(m: &FileMeta, e: &Expect) -> bool {
    match e {
        Expect::Any => true,
        Expect::Absent => false,
        Expect::Stat { size, mtime } => !m.dir && m.size == *size && m.mtime == *mtime,
    }
}

/// Условие записи не выполнено; наш временный файл убирается.
async fn refuse(src: &Path, own: bool) -> std::io::Result<IoResult> {
    if own {
        let _ = tokio::fs::remove_file(src).await;
    }
    Ok(IoResult::Precondition)
}

/// ФС не умеет жёстких ссылок (FAT/exFAT, часть сетевых и облачных ФС).
fn no_links(e: &std::io::Error) -> bool {
    use std::io::ErrorKind as K;
    matches!(
        e.kind(),
        K::PermissionDenied
            | K::Unsupported
            | K::CrossesDevices
            | K::TooManyLinks
            | K::InvalidInput
    )
}

/// Время изменения inode (мс), 0 — неизвестно (`FileMeta::ctime`).
#[cfg(unix)]
fn ctime_ms(md: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    md.ctime() * 1000 + md.ctime_nsec() / 1_000_000
}

#[cfg(not(unix))]
fn ctime_ms(_md: &std::fs::Metadata) -> i64 {
    0
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Запись с fsync: rename поверх старого файла не оставит пустышку после сбоя питания.
async fn write_durable(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut f = tokio::fs::File::create(path).await?;
    f.write_all(data).await?;
    f.sync_all().await
}

async fn read_range(
    path: &Path,
    offset: u64,
    len: Option<u64>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = f.metadata().await?.len();
    let start = offset.min(size);
    let end = len.map_or(size, |l| size.min(start.saturating_add(l)));
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut buf = vec![0; usize::try_from(end - start).unwrap_or(0)];
    f.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &str, data: &[u8], expect: Expect) -> Action {
        Action::Write {
            id: 1,
            path: path.into(),
            data: data.to_vec(),
            expect,
        }
    }

    fn stat_of(dir: &Path, p: &str) -> Expect {
        let md = std::fs::metadata(dir.join(p)).unwrap();
        Expect::Stat {
            size: md.len(),
            mtime: mtime_ms(&md),
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != STATE_DIR)
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn write_checks_condition_and_leaves_no_aside() {
        let dir = tempfile::tempdir().unwrap();
        let ex = Exec::new(dir.path(), "http://127.0.0.1:9", None).unwrap();
        std::fs::write(dir.path().join("a.md"), b"old").unwrap();
        let expect = stat_of(dir.path(), "a.md");
        // Файл уже есть — «создать» не выйдет и не затрёт его.
        assert_eq!(
            ex.perform(write("a.md", b"x", Expect::Absent)).await,
            IoResult::Precondition
        );
        let r = ex.perform(write("a.md", b"remote", expect)).await;
        let IoResult::Stat { meta: Some(m) } = r else {
            panic!("{r:?}")
        };
        assert_eq!(m.size, 6);
        assert_ne!(m.ctime, 0);
        assert_eq!(std::fs::read(dir.path().join("a.md")).unwrap(), b"remote");
        // Условие на прежнее состояние больше не выполняется.
        assert_eq!(
            ex.perform(write("a.md", b"again", expect)).await,
            IoResult::Precondition
        );
        assert_eq!(
            ex.perform(write("b.md", b"new", Expect::Absent)).await,
            IoResult::Stat {
                meta: ex.stat("b.md").await.unwrap()
            }
        );
        assert_eq!(names(dir.path()), vec!["a.md", "b.md"]);
    }

    #[tokio::test]
    async fn changed_file_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let ex = Exec::new(dir.path(), "http://127.0.0.1:9", None).unwrap();
        std::fs::write(dir.path().join("a.md"), b"old").unwrap();
        let expect = stat_of(dir.path(), "a.md");
        std::fs::write(dir.path().join("a.md"), b"user edit").unwrap();
        assert_eq!(
            ex.perform(write("a.md", b"remote", expect)).await,
            IoResult::Precondition
        );
        assert_eq!(
            std::fs::read(dir.path().join("a.md")).unwrap(),
            b"user edit"
        );
        assert_eq!(names(dir.path()), vec!["a.md"]);
    }
}
