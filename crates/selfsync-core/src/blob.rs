//! Формат хранимого блоба (раздел 9.2).
//!
//! ```text
//! заголовок (17 байт, LE):
//!   magic "NSB" | version u8 | flags u8 | chunk_size u32 | plaintext_len u64
//! открытый блоб:      заголовок | открытый текст (или lz4-блок при flags.compressed)
//! зашифрованный блоб: заголовок | content_id (16) | чанк*
//!   чанк = nonce (12) | AES-256-GCM-SIV(шифротекст ‖ тег 16)
//! ```
//!
//! Чанки по 1 МиБ шифруются независимо: память ограничена размером чанка, а куски,
//! скачанные через `Range`, расшифровываются по отдельности. Шифрование
//! детерминированное: nonce выводится из хэша открытого текста и индекса чанка, так что
//! одинаковый открытый текст даёт одинаковый блоб (дедупликация и идемпотентный повтор).
//! AAD чанка = заголовок ‖ content_id ‖ индекс ‖ флаг последнего чанка: защищает от
//! перестановки, обрезания и перестановки чанков между файлами.
//!
//! Сжатие (lz4) применяется только к тексту, только в зашифрованных блобах и только
//! если весь текст помещается в один чанк. Открытые блобы не сжимаются намеренно:
//! содержимое такого блоба — это файл после 17 байт заголовка, его можно достать из
//! резервной копии без инструментов.

use aes_gcm_siv::aead::{Aead, Payload};

use crate::crypto::VaultKeys;
use crate::hash::{Hash, Hasher};

pub const MAGIC: &[u8; 3] = b"NSB";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 17;
pub const CONTENT_ID_LEN: usize = 16;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Размер чанка по умолчанию: 1 МиБ.
pub const DEFAULT_CHUNK_SIZE: u32 = 1 << 20;

pub const FLAG_ENCRYPTED: u8 = 0b01;
pub const FLAG_COMPRESSED: u8 = 0b10;

/// Порог, ниже которого сжимать бессмысленно.
const COMPRESS_MIN: u64 = 512;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("не блоб selfsync")]
    BadMagic,
    #[error("неподдерживаемая версия формата блоба: {0}")]
    UnsupportedVersion(u8),
    #[error("неизвестные флаги блоба")]
    BadFlags,
    #[error("блоб обрезан или длина не сходится")]
    Truncated,
    #[error("блоб зашифрован, а ключа нет")]
    NeedKey,
    #[error("ошибка расшифровки: данные повреждены или подменены")]
    Decrypt,
    #[error("ошибка распаковки")]
    Decompress,
    #[error("неверный индекс чанка")]
    BadChunk,
}

/// Разобранный заголовок.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobHeader {
    pub flags: u8,
    pub chunk_size: u32,
    pub plaintext_len: u64,
}

impl BlobHeader {
    pub fn encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }

    pub fn compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..3].copy_from_slice(MAGIC);
        out[3] = VERSION;
        out[4] = self.flags;
        out[5..9].copy_from_slice(&self.chunk_size.to_le_bytes());
        out[9..17].copy_from_slice(&self.plaintext_len.to_le_bytes());
        out
    }

    pub fn parse(b: &[u8]) -> Result<BlobHeader, BlobError> {
        if b.len() < HEADER_LEN {
            return Err(BlobError::Truncated);
        }
        if &b[..3] != MAGIC {
            return Err(BlobError::BadMagic);
        }
        if b[3] != VERSION {
            return Err(BlobError::UnsupportedVersion(b[3]));
        }
        let flags = b[4];
        if flags & !(FLAG_ENCRYPTED | FLAG_COMPRESSED) != 0 {
            return Err(BlobError::BadFlags);
        }
        let chunk_size = u32::from_le_bytes([b[5], b[6], b[7], b[8]]);
        if chunk_size == 0 {
            return Err(BlobError::BadFlags);
        }
        let mut len = [0u8; 8];
        len.copy_from_slice(&b[9..17]);
        Ok(BlobHeader {
            flags,
            chunk_size,
            plaintext_len: u64::from_le_bytes(len),
        })
    }

    /// Число чанков (у пустого файла — один пустой чанк в зашифрованном виде).
    pub fn chunk_count(&self) -> u64 {
        if self.compressed() {
            return 1;
        }
        let cs = u64::from(self.chunk_size);
        self.plaintext_len.div_ceil(cs).max(1)
    }

    /// Длина открытого текста чанка `i`.
    pub fn chunk_plain_len(&self, i: u64) -> u64 {
        let cs = u64::from(self.chunk_size);
        let start = i * cs;
        self.plaintext_len.saturating_sub(start).min(cs)
    }

    /// Смещение начала данных (после заголовка и content_id).
    pub fn data_offset(&self) -> u64 {
        if self.encrypted() {
            (HEADER_LEN + CONTENT_ID_LEN) as u64
        } else {
            HEADER_LEN as u64
        }
    }

    /// Диапазон байт чанка `i` в блобе (без сжатия). Для расшифровки части
    /// файла, скачанной через `Range`.
    pub fn chunk_range(&self, i: u64) -> Result<std::ops::Range<u64>, BlobError> {
        if self.compressed() || i >= self.chunk_count() {
            return Err(BlobError::BadChunk);
        }
        let cs = u64::from(self.chunk_size);
        let overhead = if self.encrypted() {
            (NONCE_LEN + TAG_LEN) as u64
        } else {
            0
        };
        let start = self.data_offset() + i * (cs + overhead);
        Ok(start..start + self.chunk_plain_len(i) + overhead)
    }

    /// Полная длина блоба без сжатия.
    pub fn encoded_len(&self) -> u64 {
        let overhead = if self.encrypted() {
            (NONCE_LEN + TAG_LEN) as u64 * self.chunk_count()
        } else {
            0
        };
        self.data_offset() + self.plaintext_len + overhead
    }
}

/// Похоже ли содержимое на текст (UTF-8 без NUL).
pub fn is_text(data: &[u8]) -> bool {
    !data.contains(&0) && std::str::from_utf8(data).is_ok()
}

fn chunk_aad(
    header: &[u8; HEADER_LEN],
    content_id: &[u8; CONTENT_ID_LEN],
    i: u64,
    last: bool,
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(HEADER_LEN + CONTENT_ID_LEN + 9);
    aad.extend_from_slice(header);
    aad.extend_from_slice(content_id);
    aad.extend_from_slice(&i.to_le_bytes());
    aad.push(u8::from(last));
    aad
}

/// Потоковый кодировщик: открытый текст подаётся чанками по порядку.
///
/// Хэш открытого текста нужен заранее (из него выводятся nonce), поэтому большой
/// файл в зашифрованном vault'е читается дважды: сначала хэш, потом кодирование.
pub struct BlobEncoder<'k> {
    header: BlobHeader,
    header_bytes: [u8; HEADER_LEN],
    keys: Option<&'k VaultKeys>,
    plaintext_hash: Hash,
    content_id: [u8; CONTENT_ID_LEN],
    next_chunk: u64,
}

impl<'k> BlobEncoder<'k> {
    /// Кодировщик без сжатия.
    pub fn new(keys: Option<&'k VaultKeys>, plaintext_len: u64, plaintext_hash: Hash) -> Self {
        let header = BlobHeader {
            flags: if keys.is_some() { FLAG_ENCRYPTED } else { 0 },
            chunk_size: DEFAULT_CHUNK_SIZE,
            plaintext_len,
        };
        let content_id = keys
            .map(|k| k.content_id(&plaintext_hash))
            .unwrap_or_default();
        BlobEncoder {
            header_bytes: header.to_bytes(),
            header,
            keys,
            plaintext_hash,
            content_id,
            next_chunk: 0,
        }
    }

    pub fn header(&self) -> BlobHeader {
        self.header
    }

    /// Заголовок (и content_id для зашифрованного блоба) — начало блоба.
    pub fn prefix(&self) -> Vec<u8> {
        let mut out = self.header_bytes.to_vec();
        if self.keys.is_some() {
            out.extend_from_slice(&self.content_id);
        }
        out
    }

    /// Длина открытого текста следующего чанка.
    pub fn next_chunk_len(&self) -> u64 {
        self.header.chunk_plain_len(self.next_chunk)
    }

    pub fn chunks_left(&self) -> u64 {
        self.header.chunk_count().saturating_sub(self.next_chunk)
    }

    /// Кодирует следующий чанк. Длина должна совпадать с [`Self::next_chunk_len`].
    pub fn encode_chunk(&mut self, plain: &[u8]) -> Result<Vec<u8>, BlobError> {
        let out = self.encode_chunk_at(self.next_chunk, plain)?;
        self.next_chunk += 1;
        Ok(out)
    }

    /// Кодирует чанк `i` вне очереди (продолжение загрузки с середины: шифрование
    /// детерминированное, поэтому повторно закодированный чанк совпадает побайтно).
    pub fn encode_chunk_at(&self, i: u64, plain: &[u8]) -> Result<Vec<u8>, BlobError> {
        if i >= self.header.chunk_count() || plain.len() as u64 != self.header.chunk_plain_len(i) {
            return Err(BlobError::BadChunk);
        }
        match self.keys {
            None => Ok(plain.to_vec()),
            Some(keys) => {
                let last = i + 1 == self.header.chunk_count();
                Ok(encrypt_chunk(
                    keys,
                    &self.header_bytes,
                    &self.content_id,
                    &self.plaintext_hash,
                    i,
                    last,
                    plain,
                ))
            }
        }
    }
}

fn encrypt_chunk(
    keys: &VaultKeys,
    header: &[u8; HEADER_LEN],
    content_id: &[u8; CONTENT_ID_LEN],
    plaintext_hash: &Hash,
    i: u64,
    last: bool,
    plain: &[u8],
) -> Vec<u8> {
    let nonce = keys.chunk_nonce(plaintext_hash, i);
    let aad = chunk_aad(header, content_id, i, last);
    let ct = keys
        .content_cipher()
        .encrypt(
            &nonce.into(),
            Payload {
                msg: plain,
                aad: &aad,
            },
        )
        // GCM-SIV отказывает только на входе > 2^36 байт; чанк ограничен 1 МиБ.
        .unwrap_or_else(|_| unreachable!());
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Кодирует содержимое целиком (файлы, помещающиеся в память).
pub fn encode(plain: &[u8], keys: Option<&VaultKeys>) -> Vec<u8> {
    let plaintext_hash = Hash::of(plain);
    encode_with_hash(plain, plaintext_hash, keys)
}

/// То же, когда хэш открытого текста уже посчитан.
pub fn encode_with_hash(plain: &[u8], plaintext_hash: Hash, keys: Option<&VaultKeys>) -> Vec<u8> {
    let len = plain.len() as u64;
    if let Some(k) = keys
        && len >= COMPRESS_MIN
        && len <= u64::from(DEFAULT_CHUNK_SIZE)
        && is_text(plain)
    {
        let packed = lz4_flex::block::compress(plain);
        if (packed.len() as u64) * 10 < len * 9 {
            let header = BlobHeader {
                flags: FLAG_ENCRYPTED | FLAG_COMPRESSED,
                chunk_size: DEFAULT_CHUNK_SIZE,
                plaintext_len: len,
            };
            let hb = header.to_bytes();
            let cid = k.content_id(&plaintext_hash);
            let mut out = hb.to_vec();
            out.extend_from_slice(&cid);
            out.extend(encrypt_chunk(
                k,
                &hb,
                &cid,
                &plaintext_hash,
                0,
                true,
                &packed,
            ));
            return out;
        }
    }
    let mut enc = BlobEncoder::new(keys, len, plaintext_hash);
    let mut out = enc.prefix();
    out.reserve(usize::try_from(enc.header().encoded_len()).unwrap_or(0));
    let mut offset = 0usize;
    while enc.chunks_left() > 0 {
        let n = usize::try_from(enc.next_chunk_len()).unwrap_or(0);
        // Длины согласованы по построению: ошибки быть не может.
        let chunk = enc
            .encode_chunk(&plain[offset..offset + n])
            .unwrap_or_else(|_| unreachable!());
        out.extend_from_slice(&chunk);
        offset += n;
    }
    out
}

/// Хэш блоба, который получился бы из содержимого (для сравнения с сервером без
/// скачивания). Открытый блоб хэшируется без копирования.
pub fn blob_hash(plain: &[u8], plaintext_hash: Hash, keys: Option<&VaultKeys>) -> Hash {
    match keys {
        None => {
            let header = BlobHeader {
                flags: 0,
                chunk_size: DEFAULT_CHUNK_SIZE,
                plaintext_len: plain.len() as u64,
            };
            let mut h = Hasher::new();
            h.update(&header.to_bytes());
            h.update(plain);
            h.finish()
        }
        Some(_) => Hash::of(&encode_with_hash(plain, plaintext_hash, keys)),
    }
}

/// Декодирует блоб целиком.
pub fn decode(blob: &[u8], keys: Option<&VaultKeys>) -> Result<Vec<u8>, BlobError> {
    let header = BlobHeader::parse(blob)?;
    let mut out = Vec::with_capacity(
        usize::try_from(header.plaintext_len)
            .unwrap_or(0)
            .min(1 << 30),
    );
    if header.compressed() {
        if !header.encrypted() {
            // Открытые блобы не сжимаются (см. описание модуля).
            return Err(BlobError::BadFlags);
        }
        let keys = keys.ok_or(BlobError::NeedKey)?;
        let hb = header.to_bytes();
        let off = HEADER_LEN + CONTENT_ID_LEN;
        if blob.len() < off + NONCE_LEN + TAG_LEN {
            return Err(BlobError::Truncated);
        }
        let mut cid = [0u8; CONTENT_ID_LEN];
        cid.copy_from_slice(&blob[HEADER_LEN..off]);
        let packed = decrypt_chunk_raw(keys, &hb, &cid, 0, true, &blob[off..])?;
        let plain = lz4_flex::block::decompress(
            &packed,
            usize::try_from(header.plaintext_len).map_err(|_| BlobError::Truncated)?,
        )
        .map_err(|_| BlobError::Decompress)?;
        if plain.len() as u64 != header.plaintext_len {
            return Err(BlobError::Decompress);
        }
        return Ok(plain);
    }
    if header.encoded_len() != blob.len() as u64 {
        return Err(BlobError::Truncated);
    }
    let mut dec = ChunkDecoder::new(
        &blob[..usize::try_from(header.data_offset()).unwrap_or(0)],
        keys,
    )?;
    for i in 0..header.chunk_count() {
        let r = header.chunk_range(i)?;
        let part = &blob[usize::try_from(r.start).map_err(|_| BlobError::Truncated)?
            ..usize::try_from(r.end).map_err(|_| BlobError::Truncated)?];
        out.extend_from_slice(&dec.decode_chunk(i, part)?);
    }
    Ok(out)
}

fn decrypt_chunk_raw(
    keys: &VaultKeys,
    header: &[u8; HEADER_LEN],
    content_id: &[u8; CONTENT_ID_LEN],
    i: u64,
    last: bool,
    data: &[u8],
) -> Result<Vec<u8>, BlobError> {
    if data.len() < NONCE_LEN + TAG_LEN {
        return Err(BlobError::Truncated);
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&data[..NONCE_LEN]);
    let aad = chunk_aad(header, content_id, i, last);
    keys.content_cipher()
        .decrypt(
            &nonce.into(),
            Payload {
                msg: &data[NONCE_LEN..],
                aad: &aad,
            },
        )
        .map_err(|_| BlobError::Decrypt)
}

/// Декодер отдельных чанков (для потокового и частичного скачивания).
pub struct ChunkDecoder<'k> {
    header: BlobHeader,
    header_bytes: [u8; HEADER_LEN],
    content_id: [u8; CONTENT_ID_LEN],
    keys: Option<&'k VaultKeys>,
}

impl<'k> ChunkDecoder<'k> {
    /// `prefix` — первые [`BlobHeader::data_offset`] байт блоба.
    pub fn new(prefix: &[u8], keys: Option<&'k VaultKeys>) -> Result<Self, BlobError> {
        let header = BlobHeader::parse(prefix)?;
        if header.compressed() {
            return Err(BlobError::BadChunk);
        }
        let mut content_id = [0u8; CONTENT_ID_LEN];
        if header.encrypted() {
            if keys.is_none() {
                return Err(BlobError::NeedKey);
            }
            if prefix.len() < HEADER_LEN + CONTENT_ID_LEN {
                return Err(BlobError::Truncated);
            }
            content_id.copy_from_slice(&prefix[HEADER_LEN..HEADER_LEN + CONTENT_ID_LEN]);
        }
        Ok(ChunkDecoder {
            header_bytes: header.to_bytes(),
            header,
            content_id,
            keys,
        })
    }

    pub fn header(&self) -> BlobHeader {
        self.header
    }

    /// Расшифровывает чанк `i` (байты ровно из [`BlobHeader::chunk_range`]).
    pub fn decode_chunk(&mut self, i: u64, data: &[u8]) -> Result<Vec<u8>, BlobError> {
        let range = self.header.chunk_range(i)?;
        if data.len() as u64 != range.end - range.start {
            return Err(BlobError::Truncated);
        }
        match self.keys {
            None if !self.header.encrypted() => Ok(data.to_vec()),
            None => Err(BlobError::NeedKey),
            Some(_) if !self.header.encrypted() => Ok(data.to_vec()),
            Some(keys) => {
                let last = i + 1 == self.header.chunk_count();
                let plain =
                    decrypt_chunk_raw(keys, &self.header_bytes, &self.content_id, i, last, data)?;
                if plain.len() as u64 != self.header.chunk_plain_len(i) {
                    return Err(BlobError::Truncated);
                }
                Ok(plain)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::MasterKey;

    fn keys() -> VaultKeys {
        MasterKey::from_bytes([9; 32]).derive()
    }

    fn small_chunks(keys: Option<&VaultKeys>, plain: &[u8], cs: u32) -> Vec<u8> {
        // Кодирование с маленьким чанком, чтобы тестировать многочанковые блобы быстро.
        let ph = Hash::of(plain);
        let mut enc = BlobEncoder::new(keys, plain.len() as u64, ph);
        enc.header.chunk_size = cs;
        enc.header_bytes = enc.header.to_bytes();
        let mut out = enc.prefix();
        let mut off = 0;
        while enc.chunks_left() > 0 {
            let n = enc.next_chunk_len() as usize;
            out.extend(enc.encode_chunk(&plain[off..off + n]).unwrap());
            off += n;
        }
        out
    }

    #[test]
    fn plain_roundtrip_is_header_plus_bytes() {
        let data = b"hello world".to_vec();
        let b = encode(&data, None);
        assert_eq!(&b[HEADER_LEN..], &data[..]);
        assert_eq!(decode(&b, None).unwrap(), data);
        assert_eq!(blob_hash(&data, Hash::of(&data), None), Hash::of(&b));
        let empty = encode(b"", None);
        assert_eq!(empty.len(), HEADER_LEN);
        assert_eq!(decode(&empty, None).unwrap(), b"");
    }

    #[test]
    fn encrypted_roundtrip_deterministic() {
        let k = keys();
        let data = b"secret note\n".repeat(10);
        let a = encode(&data, Some(&k));
        let b = encode(&data, Some(&k));
        assert_eq!(a, b, "одинаковый открытый текст — одинаковый шифротекст");
        assert_eq!(decode(&a, Some(&k)).unwrap(), data);
        assert_eq!(blob_hash(&data, Hash::of(&data), Some(&k)), Hash::of(&a));
        assert_eq!(decode(&a, None).unwrap_err(), BlobError::NeedKey);
        let other = MasterKey::from_bytes([1; 32]).derive();
        assert_eq!(decode(&a, Some(&other)).unwrap_err(), BlobError::Decrypt);
        let empty = encode(b"", Some(&k));
        assert_eq!(decode(&empty, Some(&k)).unwrap(), b"");
    }

    #[test]
    fn compression_only_for_encrypted_text() {
        let k = keys();
        let text = "строка заметки, которая повторяется\n".repeat(200);
        let e = encode(text.as_bytes(), Some(&k));
        let h = BlobHeader::parse(&e).unwrap();
        assert!(h.compressed());
        assert!(e.len() < text.len() / 2);
        assert_eq!(decode(&e, Some(&k)).unwrap(), text.as_bytes());
        let p = encode(text.as_bytes(), None);
        assert!(!BlobHeader::parse(&p).unwrap().compressed());
        let binary: Vec<u8> = (0..2000u32).map(|i| (i % 7) as u8).collect();
        assert!(
            !BlobHeader::parse(&encode(&binary, Some(&k)))
                .unwrap()
                .compressed()
        );
    }

    #[test]
    fn multi_chunk_and_range_decoding() {
        let k = keys();
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
        for keys in [None, Some(&k)] {
            let b = small_chunks(keys, &data, 1024);
            let h = BlobHeader::parse(&b).unwrap();
            assert_eq!(h.chunk_count(), 10);
            assert_eq!(h.encoded_len(), b.len() as u64);
            assert_eq!(decode(&b, keys).unwrap(), data);
            // Чанк 7 отдельно, как после Range-запроса.
            let prefix = &b[..h.data_offset() as usize];
            let mut dec = ChunkDecoder::new(prefix, keys).unwrap();
            let r = h.chunk_range(7).unwrap();
            let part = dec
                .decode_chunk(7, &b[r.start as usize..r.end as usize])
                .unwrap();
            assert_eq!(part, &data[7 * 1024..8 * 1024]);
            let last = h.chunk_range(9).unwrap();
            assert_eq!(
                dec.decode_chunk(9, &b[last.start as usize..last.end as usize])
                    .unwrap(),
                &data[9 * 1024..]
            );
        }
    }

    #[test]
    fn tampering_is_detected() {
        let k = keys();
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 256) as u8).collect();
        let b = small_chunks(Some(&k), &data, 1024);
        let h = BlobHeader::parse(&b).unwrap();

        // порча байта
        let mut bad = b.clone();
        bad[100] ^= 0x40;
        assert_eq!(decode(&bad, Some(&k)).unwrap_err(), BlobError::Decrypt);

        // перестановка чанков 0 и 1 (одинаковой длины)
        let r0 = h.chunk_range(0).unwrap();
        let r1 = h.chunk_range(1).unwrap();
        let mut swapped = b.clone();
        let c0 = b[r0.start as usize..r0.end as usize].to_vec();
        let c1 = b[r1.start as usize..r1.end as usize].to_vec();
        swapped[r0.start as usize..r0.end as usize].copy_from_slice(&c1);
        swapped[r1.start as usize..r1.end as usize].copy_from_slice(&c0);
        assert_eq!(decode(&swapped, Some(&k)).unwrap_err(), BlobError::Decrypt);

        // обрезание: последний чанк выкинут, длина в заголовке исправлена
        let r_last = h.chunk_range(4).unwrap();
        let mut cut = b[..r_last.start as usize].to_vec();
        let mut hh = h;
        hh.plaintext_len = 4096;
        cut[..HEADER_LEN].copy_from_slice(&hh.to_bytes());
        assert_eq!(decode(&cut, Some(&k)).unwrap_err(), BlobError::Decrypt);

        // просто обрезанный блоб
        assert_eq!(
            decode(&b[..b.len() - 1], Some(&k)).unwrap_err(),
            BlobError::Truncated
        );

        // чанк из другого файла той же длины
        let other: Vec<u8> = data.iter().map(|x| x ^ 0xff).collect();
        let ob = small_chunks(Some(&k), &other, 1024);
        let mut mixed = b.clone();
        mixed[r1.start as usize..r1.end as usize]
            .copy_from_slice(&ob[r1.start as usize..r1.end as usize]);
        assert_eq!(decode(&mixed, Some(&k)).unwrap_err(), BlobError::Decrypt);
    }

    #[test]
    fn header_validation() {
        assert_eq!(BlobHeader::parse(b"XYZ").unwrap_err(), BlobError::Truncated);
        let mut b = encode(b"x", None);
        b[0] = b'X';
        assert_eq!(BlobHeader::parse(&b).unwrap_err(), BlobError::BadMagic);
        let mut v = encode(b"x", None);
        v[3] = 2;
        assert_eq!(
            BlobHeader::parse(&v).unwrap_err(),
            BlobError::UnsupportedVersion(2)
        );
        let mut f = encode(b"x", None);
        f[4] = 0x80;
        assert_eq!(BlobHeader::parse(&f).unwrap_err(), BlobError::BadFlags);
    }

    /// Блоб формата v1, записанный при создании формата. Если тест упал, значит
    /// формат изменился несовместимо и старые блобы перестанут читаться.
    #[test]
    fn format_v1_compat_fixture() {
        let plain = encode(b"fixture", None);
        assert_eq!(
            Hash::of(&plain).to_hex(),
            Hash::of(
                &[
                    &b"NSB\x01\x00\x00\x00\x10\x00\x07\x00\x00\x00\x00\x00\x00\x00"[..],
                    b"fixture"
                ]
                .concat()
            )
            .to_hex()
        );
        let k = MasterKey::from_bytes([0x42; 32]).derive();
        let e = encode(b"fixture", Some(&k));
        assert_eq!(&e[..5], b"NSB\x01\x01");
        assert_eq!(
            e.len(),
            HEADER_LEN + CONTENT_ID_LEN + NONCE_LEN + 7 + TAG_LEN
        );
        assert_eq!(decode(&e, Some(&k)).unwrap(), b"fixture");
        // Побайтные эталоны: детерминированное шифрование обязано давать ровно это.
        assert_eq!(
            Hash::of(&e).to_hex(),
            "b42e2492a06e9d4629c6a290782ef82d54d2f431adc136480d403b6ca54d3e93"
        );
        let compressed = encode("строка\n".repeat(100).as_bytes(), Some(&k));
        assert_eq!(
            Hash::of(&compressed).to_hex(),
            "d6c4909f967b2d1d73780b8ae7bc76b64bd56f1bdd0ce8946473f60ab2b5c20e"
        );
    }
}
