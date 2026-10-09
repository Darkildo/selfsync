//! Содержимое файлов: хэширование, сборка блобов, загрузка и скачивание.
//!
//! Маленькие блобы (≤ 8 МиБ) ходят одним запросом и целиком в памяти. Большие —
//! частями: загрузка через resumable `uploads`, скачивание через `Range` во временный
//! файл. Состояние передачи чекпоинтится в индексе: после засыпания телефона передача
//! продолжается с принятого смещения.

use super::api;
use super::ctx::{Ctx, PART, READ_CHUNK, SMALL_BLOB, SyncError, SyncResult, TEMP_SUFFIX};
use super::types::{Expect, FileMeta};
use crate::blob::{self, BlobEncoder, BlobHeader, ChunkDecoder, DEFAULT_CHUNK_SIZE};
use crate::crypto::VaultKeys;
use crate::hash::{Hash, Hasher};
use crate::index::{LocalObs, Transfer};

/// Откуда читать содержимое при отправке: файл vault'а или временный файл
/// (миграция больших файлов, которых нет локально).
#[derive(Clone, Copy)]
pub(crate) enum Src<'a> {
    Vault(&'a str),
    Temp(&'a str),
}

impl Src<'_> {
    async fn read(&self, cx: &Ctx, offset: u64, len: u64) -> SyncResult<Option<Vec<u8>>> {
        match self {
            Src::Vault(p) => cx.read(p, offset, Some(len)).await,
            Src::Temp(t) => cx.read_temp(t, offset, len).await,
        }
    }
}

/// Результат чтения локального файла.
pub(crate) struct LocalContent {
    pub obs: LocalObs,
    /// Содержимое, если файл небольшой.
    pub data: Option<Vec<u8>>,
}

/// Читает и хэширует локальный файл. `None` — файла нет.
pub(crate) async fn read_local(cx: &Ctx, path: &str, meta: &FileMeta) -> SyncResult<Option<LocalContent>> {
    if meta.size <= SMALL_BLOB {
        let Some(data) = cx.read(path, 0, None).await? else {
            return Ok(None);
        };
        let obs = LocalObs {
            size: data.len() as u64,
            mtime: meta.mtime,
            plain: Hash::of(&data),
        };
        return Ok(Some(LocalContent { obs, data: Some(data) }));
    }
    let mut h = Hasher::new();
    let mut off = 0u64;
    while off < meta.size {
        let len = READ_CHUNK.min(meta.size - off);
        let Some(chunk) = cx.read(path, off, Some(len)).await? else {
            return Ok(None);
        };
        if chunk.is_empty() {
            break;
        }
        h.update(&chunk);
        off += chunk.len() as u64;
    }
    Ok(Some(LocalContent {
        obs: LocalObs {
            size: off,
            mtime: meta.mtime,
            plain: h.finish(),
        },
        data: None,
    }))
}

/// Длина блоба для открытого текста длины `len` без сжатия.
pub(crate) fn blob_len(len: u64, encrypted: bool) -> u64 {
    BlobHeader {
        flags: if encrypted { blob::FLAG_ENCRYPTED } else { 0 },
        chunk_size: DEFAULT_CHUNK_SIZE,
        plaintext_len: len,
    }
    .encoded_len()
}

/// Подготовленный к отправке блоб.
pub(crate) struct PreparedBlob {
    pub hash: Hash,
    pub len: u64,
    /// Байты блоба, если он маленький.
    pub data: Option<Vec<u8>>,
}

/// Строит блоб маленького файла в памяти.
pub(crate) fn prepare_small(plain: &[u8], plain_hash: Hash, keys: Option<&VaultKeys>) -> PreparedBlob {
    let data = blob::encode_with_hash(plain, plain_hash, keys);
    PreparedBlob {
        hash: Hash::of(&data),
        len: data.len() as u64,
        data: Some(data),
    }
}

/// Хэш блоба большого файла (повторное чтение файла частями).
pub(crate) async fn prepare_big(cx: &Ctx, src: Src<'_>, obs: &LocalObs, keys: Option<&VaultKeys>) -> SyncResult<Option<PreparedBlob>> {
    let len = blob_len(obs.size, keys.is_some());
    let mut h = Hasher::new();
    let enc = BlobEncoder::new(keys, obs.size, obs.plain);
    h.update(&enc.prefix());
    let header = enc.header();
    let mut plain_check = Hasher::new();
    for i in 0..header.chunk_count() {
        let clen = header.chunk_plain_len(i);
        let Some(chunk) = src.read(cx, i * u64::from(header.chunk_size), clen).await? else {
            return Ok(None);
        };
        if chunk.len() as u64 != clen {
            return Ok(None);
        }
        plain_check.update(&chunk);
        let encoded = enc.encode_chunk_at(i, &chunk).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        h.update(&encoded);
    }
    if plain_check.finish() != obs.plain {
        // Файл изменился между сканированием и отправкой.
        return Ok(None);
    }
    Ok(Some(PreparedBlob {
        hash: h.finish(),
        len,
        data: None,
    }))
}

/// Байты блоба большого файла в диапазоне `[offset, offset + len)`.
async fn blob_range(cx: &Ctx, src: Src<'_>, obs: &LocalObs, keys: Option<&VaultKeys>, offset: u64, len: u64) -> SyncResult<Option<Vec<u8>>> {
    let enc = BlobEncoder::new(keys, obs.size, obs.plain);
    let header = enc.header();
    let prefix = enc.prefix();
    let end = (offset + len).min(header.encoded_len());
    let mut out = Vec::with_capacity(usize::try_from(end - offset).unwrap_or(0));
    let mut pos = offset;
    if pos < prefix.len() as u64 {
        let p_end = end.min(prefix.len() as u64);
        out.extend_from_slice(&prefix[usize::try_from(pos).unwrap_or(0)..usize::try_from(p_end).unwrap_or(0)]);
        pos = p_end;
    }
    while pos < end {
        // Чанк, в который попадает pos.
        let data_off = pos - header.data_offset();
        let overhead = header.chunk_range(0).map(|r| r.end - r.start - header.chunk_plain_len(0)).unwrap_or(0);
        let stride = u64::from(header.chunk_size) + overhead;
        let i = data_off / stride;
        let r = header.chunk_range(i).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        let clen = header.chunk_plain_len(i);
        let Some(chunk) = src.read(cx, i * u64::from(header.chunk_size), clen).await? else {
            return Ok(None);
        };
        if chunk.len() as u64 != clen {
            return Ok(None);
        }
        let encoded = enc.encode_chunk_at(i, &chunk).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        let from = usize::try_from(pos - r.start).unwrap_or(0);
        let to = usize::try_from(end.min(r.end) - r.start).unwrap_or(0);
        out.extend_from_slice(&encoded[from..to]);
        pos = end.min(r.end);
    }
    Ok(Some(out))
}

/// Загружает маленький блоб.
pub(crate) async fn upload_small(cx: &Ctx, b: &PreparedBlob) -> SyncResult<()> {
    let data = b.data.clone().unwrap_or_default();
    api::put_blob(cx, &b.hash, data).await
}

/// Загружает большой блоб частями с продолжением. `Ok(false)` — файл изменился.
/// `key` — путь индекса, в котором чекпоинтится состояние загрузки.
pub(crate) async fn upload_big(cx: &Ctx, key: &str, src: Src<'_>, obs: &LocalObs, b: &PreparedBlob, keys: Option<&VaultKeys>) -> SyncResult<bool> {
    let st = api::upload_start(cx, &b.hash, b.len).await?;
    if st.complete {
        return Ok(true);
    }
    let id = st.upload_id.clone();
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.transfer = Some(Transfer::Upload {
                blob: b.hash,
                upload_id: id.clone(),
                plain: obs.plain,
            });
        }
    });
    cx.save().await?;
    let mut offset = st.offset;
    let mut stalls = 0;
    while offset < b.len {
        let len = PART.min(b.len - offset);
        let Some(part) = blob_range(cx, src, obs, keys, offset, len).await? else {
            return Ok(false);
        };
        match api::upload_part(cx, &id, offset, b.len, part).await? {
            Some(s) => {
                offset = s.offset;
                stalls = 0;
            }
            None => {
                // Сервер ждёт другое смещение (часть дошла, ответ — нет).
                offset = api::upload_state(cx, &id).await?.offset;
                stalls += 1;
                if stalls > 5 {
                    return Err(SyncError::Protocol("загрузка не продвигается".into()));
                }
            }
        }
        cx.set_status(|_| {});
    }
    match api::upload_commit(cx, &id).await {
        Ok(_) => {}
        Err(SyncError::Http { code, .. }) if code == "hash_mismatch" => return Ok(false),
        Err(e) => return Err(e),
    }
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.transfer = None;
        }
    });
    Ok(true)
}

/// Скачивает маленький блоб и возвращает открытый текст (с проверкой хэша).
pub(crate) async fn fetch_small(cx: &Ctx, h: &Hash) -> SyncResult<Vec<u8>> {
    let data = api::get_blob(cx, h, None).await?;
    if Hash::of(&data) != *h {
        return Err(SyncError::Corrupt("хэш скачанного блоба не совпал".into()));
    }
    let keys = cx.with(|s| s.keys.clone());
    let header = BlobHeader::parse(&data).map_err(|e| SyncError::Corrupt(e.to_string()))?;
    if header.encrypted() && keys.is_none() {
        return Err(SyncError::Paused("need_password".into()));
    }
    blob::decode(&data, if header.encrypted() { keys.as_ref() } else { None })
        .map_err(|e| SyncError::Corrupt(e.to_string()))
}

/// Скачивает блоб и атомарно кладёт открытый текст на место.
/// `Ok(None)` — условие записи не выполнено (локальный файл изменился).
pub(crate) async fn download_to(cx: &Ctx, key: &str, h: &Hash, blob_size: u64, expect: Expect) -> SyncResult<Option<(LocalObs, Option<Vec<u8>>)>> {
    if blob_size <= SMALL_BLOB {
        let plain = fetch_small(cx, h).await?;
        let plain_hash = Hash::of(&plain);
        let size = plain.len() as u64;
        let keep = if crate::merge::mergeable(&plain) { Some(plain.clone()) } else { None };
        let Some(meta) = cx.write(key, plain, expect).await? else {
            return Ok(None);
        };
        return Ok(Some((
            LocalObs {
                size,
                mtime: meta.mtime,
                plain: plain_hash,
            },
            keep,
        )));
    }
    download_big(cx, key, h, blob_size, Some(expect)).await.map(|o| o.map(|obs| (obs, None)))
}

/// Скачивает большой блоб во временный файл (без переноса в vault). Возвращает имя
/// временного файла и наблюдение открытого текста.
pub(crate) async fn download_temp(cx: &Ctx, h: &Hash, blob_size: u64) -> SyncResult<(String, LocalObs)> {
    let obs = download_big(cx, "", h, blob_size, None)
        .await?
        .ok_or_else(|| SyncError::Io("временный файл не записан".into()))?;
    Ok((temp_name(h), obs))
}

fn temp_name(h: &Hash) -> String {
    format!("dl-{}{TEMP_SUFFIX}", &h.to_hex()[..24])
}

async fn download_big(cx: &Ctx, key: &str, h: &Hash, blob_size: u64, expect: Option<Expect>) -> SyncResult<Option<LocalObs>> {
    let keys = cx.with(|s| s.keys.clone());
    // Заголовок и content_id.
    let head = api::get_blob(cx, h, Some((0, blob::HEADER_LEN as u64 + blob::CONTENT_ID_LEN as u64 - 1))).await?;
    let header = BlobHeader::parse(&head).map_err(|e| SyncError::Corrupt(e.to_string()))?;
    if header.encoded_len() != blob_size || header.compressed() {
        return Err(SyncError::Corrupt("размер блоба не сходится с заголовком".into()));
    }
    let prefix_len = usize::try_from(header.data_offset()).unwrap_or(0);
    if head.len() < prefix_len {
        return Err(SyncError::Corrupt("короткий заголовок".into()));
    }
    let mut dec = ChunkDecoder::new(&head[..prefix_len], if header.encrypted() { keys.as_ref() } else { None })
        .map_err(|e| match e {
            blob::BlobError::NeedKey => SyncError::Paused("need_password".into()),
            e => SyncError::Corrupt(e.to_string()),
        })?;

    // Продолжение прерванного скачивания.
    let temp = temp_name(h);
    let resume = cx.with(|s| match s.index.files.get(key).and_then(|f| f.transfer.clone()) {
        Some(Transfer::Download { blob, temp: t, blob_offset, plain_offset }) if blob == *h && t == temp => Some((blob_offset, plain_offset)),
        _ => None,
    });
    let mut plain_hasher = Hasher::new();
    let mut blob_hasher = Hasher::new();
    blob_hasher.update(&head[..prefix_len]);
    let (mut chunk_i, mut plain_off) = (0u64, 0u64);
    if let Some((boff, poff)) = resume {
        // Пересчитать хэши по уже записанному.
        let mut off = 0;
        let mut ok = true;
        while off < poff {
            let len = READ_CHUNK.min(poff - off);
            match cx.read_temp(&temp, off, len).await? {
                Some(d) if d.len() as u64 == len => {
                    plain_hasher.update(&d);
                    blob_hasher.update(&d);
                    off += len;
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            plain_off = poff;
            // boff всегда на границе чанка.
            let stride = header.chunk_range(0).map(|r| r.end - r.start).unwrap_or(1);
            chunk_i = (boff - header.data_offset()) / stride.max(1);
        } else {
            plain_hasher = Hasher::new();
            blob_hasher = Hasher::new();
            blob_hasher.update(&head[..prefix_len]);
            cx.delete_temp(&temp).await?;
        }
    } else {
        cx.delete_temp(&temp).await?;
    }

    let total_chunks = header.chunk_count();
    // Сколько чанков в одном Range-запросе.
    let per_part = (PART / u64::from(header.chunk_size)).max(1);
    while chunk_i < total_chunks {
        let last = (chunk_i + per_part).min(total_chunks) - 1;
        let start = header.chunk_range(chunk_i).map_err(|e| SyncError::Corrupt(e.to_string()))?.start;
        let end = header.chunk_range(last).map_err(|e| SyncError::Corrupt(e.to_string()))?.end;
        let data = api::get_blob(cx, h, Some((start, end - 1))).await?;
        if data.len() as u64 != end - start {
            return Err(SyncError::Corrupt("Range вернул не тот размер".into()));
        }
        let mut plain_part = Vec::with_capacity(usize::try_from(per_part * u64::from(header.chunk_size)).unwrap_or(0));
        for i in chunk_i..=last {
            let r = header.chunk_range(i).map_err(|e| SyncError::Corrupt(e.to_string()))?;
            let slice = &data[usize::try_from(r.start - start).unwrap_or(0)..usize::try_from(r.end - start).unwrap_or(0)];
            if !header.encrypted() {
                blob_hasher.update(slice);
            }
            let p = dec.decode_chunk(i, slice).map_err(|e| SyncError::Corrupt(e.to_string()))?;
            plain_part.extend_from_slice(&p);
        }
        plain_hasher.update(&plain_part);
        let n = plain_part.len() as u64;
        cx.write_temp(&temp, plain_off, plain_part).await?;
        plain_off += n;
        chunk_i = last + 1;
        let boff = if chunk_i < total_chunks {
            header.chunk_range(chunk_i).map(|r| r.start).unwrap_or(end)
        } else {
            end
        };
        // Чекпоинт. Новому файлу заводится запись только с передачей (без базы и без
        // локального файла): push такие пропускает, а повторный pull продолжит
        // скачивание с этого места.
        if !key.is_empty() {
            cx.with_mut(|s| {
                s.index.files.entry(key.to_owned()).or_default().transfer = Some(Transfer::Download {
                    blob: *h,
                    temp: temp.clone(),
                    blob_offset: boff,
                    plain_offset: plain_off,
                });
            });
            cx.save().await?;
        }
    }
    if !header.encrypted() && blob_hasher.finish() != *h {
        cx.delete_temp(&temp).await?;
        return Err(SyncError::Corrupt("хэш скачанного блоба не совпал".into()));
    }
    let plain = plain_hasher.finish();
    let Some(expect) = expect else {
        return Ok(Some(LocalObs {
            size: plain_off,
            mtime: 0,
            plain,
        }));
    };
    let Some(meta) = cx.commit_temp(&temp, key, expect).await? else {
        return Ok(None);
    };
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.transfer = None;
        }
    });
    Ok(Some(LocalObs {
        size: plain_off,
        mtime: meta.mtime,
        plain,
    }))
}
