//! Исполнитель действий ядра на обычной ФС и HTTP (reqwest). Контракт — как у
//! исполнителя плагина: условия записи, Precondition/NotFound, настоящие имена.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use notesync_core::engine::{Action, Expect, FileMeta, HttpRequest, IoResult};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::config::{STATE_DIR, TRASH_DIR};

const TEMP_SUFFIX: &str = ".notesync-tmp";

pub struct Exec {
    root: PathBuf,
    state: PathBuf,
    server: String,
    token: Option<String>,
    http: reqwest::Client,
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
        Ok(Exec {
            root: root.to_path_buf(),
            state: root.join(STATE_DIR),
            server: server.trim_end_matches('/').to_owned(),
            token,
            http,
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
            } => match read_range(&self.abs(&path), offset, len).await? {
                Some(data) => IoResult::Data { data },
                None => IoResult::NotFound,
            },
            Action::Write {
                path, data, expect, ..
            } => {
                if !self.holds(&path, &expect).await?
                    || self.stat(&path).await?.is_some_and(|m| m.dir)
                {
                    return Ok(IoResult::Precondition);
                }
                let target = self.abs(&path);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let tmp = PathBuf::from(format!("{}{TEMP_SUFFIX}", target.display()));
                write_durable(&tmp, &data).await?;
                tokio::fs::rename(&tmp, &target).await?;
                IoResult::Stat {
                    meta: self.stat(&path).await?,
                }
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
                if !self.holds(&path, &expect).await? {
                    return Ok(IoResult::Precondition);
                }
                let target = self.abs(&path);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::rename(self.temp(&temp), &target).await?;
                IoResult::Stat {
                    meta: self.stat(&path).await?,
                }
            }
            Action::DeleteTemp { temp, .. } => {
                match tokio::fs::remove_file(self.temp(&temp)).await {
                    Err(e) if !not_found(&e) => return Err(e),
                    _ => {}
                }
                IoResult::Done
            }
            Action::Trash { path, expect, .. } => {
                let Some(cur) = self.stat(&path).await? else {
                    return Ok(IoResult::NotFound);
                };
                if cur.dir || !matches(&cur, &expect) {
                    return Ok(IoResult::Precondition);
                }
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
                tokio::fs::create_dir_all(self.abs(&path)).await?;
                IoResult::Done
            }
            Action::Rmdir { path, .. } => match tokio::fs::remove_dir(self.abs(&path)).await {
                Ok(()) => IoResult::Done,
                Err(e) if not_found(&e) => IoResult::NotFound,
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    IoResult::Precondition
                }
                Err(e) => return Err(e),
            },
            Action::SaveIndex { data, .. } => {
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
                    Err(e) => failed(e),
                }
            }
            Err(e) => failed(e),
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
                    });
                    stack.push(path);
                } else if ft.is_file() {
                    let md = e.metadata().await?;
                    out.push(FileMeta {
                        path,
                        size: md.len(),
                        mtime: mtime_ms(&md),
                        dir: false,
                    });
                }
                // Символические ссылки не синхронизируются.
            }
        }
        Ok(out)
    }

    /// Метаданные с настоящим именем: на регистронезависимой ФС «Note.md» может
    /// оказаться «note.md» — ядро обязано это видеть.
    async fn stat(&self, path: &str) -> std::io::Result<Option<FileMeta>> {
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
            }
        } else {
            FileMeta {
                path: real,
                size: md.len(),
                mtime: mtime_ms(&md),
                dir: false,
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

    /// В локальную корзину `.trash/` (как делает Obsidian), не стирая.
    async fn trash(&self, path: &str) -> std::io::Result<()> {
        let dir = self.root.join(TRASH_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        let base = path.replace('/', " - ");
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
