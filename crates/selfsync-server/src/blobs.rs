//! Хранилище блобов, адресуемых sha256: `blobs/ab/<hex>`.
//!
//! Запись всегда идёт во временный файл в `uploads/`, затем проверка хэша, `fsync`,
//! атомарный `rename` в `blobs/` и `fsync` каталога. Существующий блоб с тем же хэшем —
//! no-op. Сервер никогда не интерпретирует содержимое.

use std::io;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use selfsync_core::hash::{Hash, Hasher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

/// Результат потоковой записи.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Записано и проверено.
    Stored { created: bool },
    /// Хэш не совпал.
    HashMismatch,
    /// Тело больше лимита.
    TooLarge,
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] io::Error),
    #[error("тело запроса оборвалось")]
    Body,
}

impl BlobStore {
    pub fn new(vault_dir: &Path) -> BlobStore {
        BlobStore {
            root: vault_dir.to_owned(),
        }
    }

    pub fn ensure_dirs(&self) -> io::Result<()> {
        std::fs::create_dir_all(self.root.join("blobs"))?;
        std::fs::create_dir_all(self.uploads_dir())?;
        Ok(())
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub fn uploads_dir(&self) -> PathBuf {
        self.root.join("uploads")
    }

    pub fn path_of(&self, h: &Hash) -> PathBuf {
        let hex = h.to_hex();
        self.root.join("blobs").join(&hex[..2]).join(hex)
    }

    pub fn upload_path(&self, id: &str) -> PathBuf {
        self.uploads_dir().join(id)
    }

    pub fn exists(&self, h: &Hash) -> bool {
        self.path_of(h).is_file()
    }

    pub fn size_of(&self, h: &Hash) -> Option<u64> {
        std::fs::metadata(self.path_of(h)).ok().map(|m| m.len())
    }

    fn temp_path(&self) -> io::Result<PathBuf> {
        let mut r = [0u8; 12];
        getrandom::fill(&mut r).map_err(|e| io::Error::other(e.to_string()))?;
        let name: String = r.iter().map(|b| format!("{b:02x}")).collect();
        Ok(self.uploads_dir().join(format!("tmp-{name}")))
    }

    /// Переносит проверенный временный файл в `blobs/` атомарно.
    pub fn finalize(&self, tmp: &Path, h: &Hash) -> io::Result<bool> {
        let dst = self.path_of(h);
        if dst.is_file() {
            let _ = std::fs::remove_file(tmp);
            return Ok(false);
        }
        let dir = dst
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.blobs_dir());
        std::fs::create_dir_all(&dir)?;
        std::fs::File::open(tmp)?.sync_all()?;
        std::fs::rename(tmp, &dst)?;
        sync_dir(&dir)?;
        Ok(true)
    }

    /// Пишет поток байтов как блоб с проверкой хэша и лимита.
    pub async fn put_stream<S, E>(
        &self,
        h: &Hash,
        mut body: S,
        limit: u64,
    ) -> Result<WriteOutcome, StreamError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
    {
        if self.exists(h) {
            return Ok(WriteOutcome::Stored { created: false });
        }
        let tmp = self.temp_path()?;
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Hasher::new();
        let mut total = 0u64;
        let res = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|_| StreamError::Body)?;
                total += chunk.len() as u64;
                if total > limit {
                    return Ok(Some(WriteOutcome::TooLarge));
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            Ok::<_, StreamError>(None)
        }
        .await;
        drop(file);
        match res {
            Ok(Some(outcome)) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(outcome)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
            Ok(None) => {
                if hasher.finish() != *h {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(WriteOutcome::HashMismatch);
                }
                let store = self.clone();
                let hh = *h;
                let created = tokio::task::spawn_blocking(move || store.finalize(&tmp, &hh))
                    .await
                    .map_err(|e| io::Error::other(e.to_string()))??;
                Ok(WriteOutcome::Stored { created })
            }
        }
    }

    /// Синхронная запись блоба целиком (импорт, тесты).
    pub fn put_bytes(&self, data: &[u8]) -> io::Result<Hash> {
        let h = Hash::of(data);
        if self.exists(&h) {
            return Ok(h);
        }
        let tmp = self.temp_path()?;
        std::fs::write(&tmp, data)?;
        self.finalize(&tmp, &h)?;
        Ok(h)
    }

    /// Дописывает часть resumable-загрузки, начиная с `offset`. Возвращает новое
    /// смещение. Оборванное тело оставляет принятые байты: клиент продолжит с них.
    pub async fn append_part<S, E>(
        &self,
        id: &str,
        offset: u64,
        mut body: S,
        max_total: u64,
        max_part: u64,
    ) -> Result<AppendOutcome, StreamError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
    {
        let path = self.upload_path(id);
        let std_file = std::fs::OpenOptions::new().append(true).open(&path)?;
        if !try_lock(&std_file) {
            return Ok(AppendOutcome::Busy);
        }
        let current = std_file.metadata()?.len();
        if current != offset {
            return Ok(AppendOutcome::OffsetMismatch { current });
        }
        let mut file = tokio::fs::File::from_std(std_file);
        let mut written = 0u64;
        let mut broken = false;
        while let Some(chunk) = body.next().await {
            let Ok(chunk) = chunk else {
                broken = true;
                break;
            };
            if written + chunk.len() as u64 > max_part
                || offset + written + chunk.len() as u64 > max_total
            {
                file.flush().await?;
                file.sync_data().await?;
                return Ok(AppendOutcome::TooLarge {
                    current: offset + written,
                });
            }
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        file.sync_data().await?;
        if broken {
            return Err(StreamError::Body);
        }
        Ok(AppendOutcome::Appended {
            offset: offset + written,
        })
    }

    /// Хэширует файл загрузки (для commit).
    pub async fn hash_file(path: &Path) -> io::Result<(Hash, u64)> {
        let mut f = tokio::fs::File::open(path).await?;
        let mut h = Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            total += n as u64;
        }
        Ok((h.finish(), total))
    }

    /// Удаляет блоб (вызывающий отвечает за проверку ссылок под блокировкой БД).
    pub fn remove(&self, h: &Hash) -> io::Result<bool> {
        match std::fs::remove_file(self.path_of(h)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Все блобы: (хэш, размер, mtime в мс).
    pub fn list(&self) -> io::Result<Vec<(Hash, u64, i64)>> {
        let mut out = Vec::new();
        let dir = self.blobs_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(out);
        };
        for sub in rd {
            let sub = sub?;
            if !sub.file_type()?.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(sub.path())? {
                let f = f?;
                let name = f.file_name();
                let Some(h) = name.to_str().and_then(Hash::from_hex) else {
                    continue;
                };
                let md = f.metadata()?;
                out.push((h, md.len(), mtime_ms(&md)));
            }
        }
        Ok(out)
    }

    /// Общий размер блобов.
    pub fn total_size(&self) -> io::Result<(u64, u64)> {
        let list = self.list()?;
        Ok((list.iter().map(|x| x.1).sum(), list.len() as u64))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended { offset: u64 },
    OffsetMismatch { current: u64 },
    TooLarge { current: u64 },
    Busy,
}

pub fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `fsync` каталога после rename (на unix; на других ОС — no-op).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Неблокирующая эксклюзивная блокировка файла (две части одной загрузки из разных
/// CGI-процессов не должны писаться одновременно). Снимается при закрытии файла.
#[cfg(unix)]
fn try_lock(f: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock с валидным открытым дескриптором не трогает память процесса.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(not(unix))]
fn try_lock(_f: &std::fs::File) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let d = tempfile::tempdir().unwrap();
        let s = BlobStore::new(d.path());
        s.ensure_dirs().unwrap();
        (d, s)
    }

    fn stream(parts: Vec<&'static [u8]>) -> impl Stream<Item = Result<Bytes, io::Error>> + Unpin {
        futures_util::stream::iter(parts.into_iter().map(|p| Ok(Bytes::from_static(p))))
    }

    #[tokio::test]
    async fn put_verify_and_dedupe() {
        let (_d, s) = store();
        let h = Hash::of(b"hello world");
        assert_eq!(
            s.put_stream(&h, stream(vec![b"hello ", b"world"]), 100)
                .await
                .unwrap(),
            WriteOutcome::Stored { created: true }
        );
        assert!(s.exists(&h));
        assert_eq!(std::fs::read(s.path_of(&h)).unwrap(), b"hello world");
        assert_eq!(
            s.put_stream(&h, stream(vec![b"whatever"]), 100)
                .await
                .unwrap(),
            WriteOutcome::Stored { created: false }
        );
        let other = Hash::of(b"x");
        assert_eq!(
            s.put_stream(&other, stream(vec![b"y"]), 100).await.unwrap(),
            WriteOutcome::HashMismatch
        );
        assert!(!s.exists(&other));
        assert_eq!(
            s.put_stream(&Hash::of(b"0123456789"), stream(vec![b"0123456789"]), 5)
                .await
                .unwrap(),
            WriteOutcome::TooLarge
        );
        // временные файлы не остаются
        assert_eq!(std::fs::read_dir(s.uploads_dir()).unwrap().count(), 0);
        assert_eq!(s.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn append_parts() {
        let (_d, s) = store();
        std::fs::write(s.upload_path("u1"), b"").unwrap();
        assert_eq!(
            s.append_part("u1", 0, stream(vec![b"abc"]), 10, 10)
                .await
                .unwrap(),
            AppendOutcome::Appended { offset: 3 }
        );
        assert_eq!(
            s.append_part("u1", 0, stream(vec![b"zzz"]), 10, 10)
                .await
                .unwrap(),
            AppendOutcome::OffsetMismatch { current: 3 }
        );
        assert_eq!(
            s.append_part("u1", 3, stream(vec![b"defghijkl"]), 10, 100)
                .await
                .unwrap(),
            AppendOutcome::TooLarge { current: 3 }
        );
        assert_eq!(
            s.append_part("u1", 3, stream(vec![b"def"]), 10, 10)
                .await
                .unwrap(),
            AppendOutcome::Appended { offset: 6 }
        );
        let (h, n) = BlobStore::hash_file(&s.upload_path("u1")).await.unwrap();
        assert_eq!((h, n), (Hash::of(b"abcdef"), 6));
    }

    #[tokio::test]
    async fn broken_body_keeps_received_bytes() {
        let (_d, s) = store();
        std::fs::write(s.upload_path("u2"), b"").unwrap();
        let parts: Vec<Result<Bytes, io::Error>> = vec![
            Ok(Bytes::from_static(b"12345")),
            Err(io::Error::other("обрыв")),
        ];
        let r = s
            .append_part("u2", 0, futures_util::stream::iter(parts), 100, 100)
            .await;
        assert!(matches!(r, Err(StreamError::Body)));
        assert_eq!(std::fs::read(s.upload_path("u2")).unwrap(), b"12345");
    }
}
