//! Локальный индекс клиента (раздел 8.2): на каждый путь — база (ревизия на сервере,
//! от которой сделана локальная версия) и последнее наблюдение локального файла.
//!
//! Хранится бинарным снимком (postcard) с версией формата и сохраняется после
//! каждого файла: процесс могут убить в любой момент.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hash::Hash;

/// Версия формата снимка. Старые версии читаются и поднимаются в [`Index::decode`].
pub const INDEX_FORMAT: u32 = 1;
const MAGIC: &[u8; 4] = b"NSI\0";

/// Режим vault'а с точки зрения клиента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VaultMode {
    #[default]
    Plain,
    Encrypted {
        key_version: u64,
    },
}

/// Наблюдение локального файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalObs {
    pub size: u64,
    pub mtime: i64,
    /// sha256 открытого текста.
    pub plain: Hash,
}

/// Незавершённая передача большого файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transfer {
    /// Загрузка блоба на сервер.
    Upload {
        blob: Hash,
        upload_id: String,
        /// Открытый текст, из которого строится блоб (если файл изменится — заново).
        plain: Hash,
    },
    /// Скачивание в временный файл.
    Download {
        blob: Hash,
        temp: String,
        /// Смещение в блобе (на границе чанка).
        blob_offset: u64,
        /// Сколько открытого текста уже записано во временный файл.
        plain_offset: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileState {
    pub folder: bool,
    /// Ревизия на сервере, от которой сделана локальная версия; 0 — неизвестна
    /// (новый файл или нужно сверить с сервером).
    pub base_rev: u64,
    /// Хэш блоба на сервере для `base_rev`.
    pub base_blob: Option<Hash>,
    /// Хэш открытого текста базы: без него 3-way merge невозможен.
    pub base_plain: Option<Hash>,
    /// Последнее наблюдение локального файла; `None` — файла локально нет.
    pub local: Option<LocalObs>,
    /// Путь на сервере, если файл переименован локально и переименование ещё не
    /// отправлено.
    pub server_path: Option<String>,
    /// Сервер отклонил эту версию (код отказа и хэш версии): не повторять, пока файл
    /// не изменится.
    pub rejected: Option<(String, Hash)>,
    pub transfer: Option<Transfer>,
    /// Put (или Mkdir) отправлен, а ответ не получен: файл, возможно, уже на сервере
    /// с этим содержимым (хэш открытого текста).
    pub pending_put: Option<Hash>,
}

impl FileState {
    /// Файл, возможно, существует на сервере.
    pub fn maybe_on_server(&self) -> bool {
        self.base_rev > 0 || self.base_plain.is_some() || self.pending_put.is_some()
    }

    /// Нужна ли отправка содержимого (Put).
    pub fn content_dirty(&self) -> bool {
        match (&self.local, self.folder) {
            (Some(l), false) => {
                if let Some((_, h)) = &self.rejected {
                    if *h == l.plain {
                        return false;
                    }
                }
                match (self.base_rev, self.base_plain) {
                    // Новый файл.
                    (0, None) => true,
                    // Ревизия неизвестна (после сброса баз), но база известна: отправлять,
                    // только если файл изменён; иначе судьбу решит pull.
                    (_, base) => base != Some(l.plain),
                }
            }
            _ => false,
        }
    }

    /// Файл удалён локально, а на сервере ещё жив (или, после сброса баз, может
    /// быть жив — тогда ревизию уточнит конфликт).
    pub fn delete_pending(&self) -> bool {
        self.local.is_none() && (self.base_rev > 0 || self.pending_put.is_some() || (!self.folder && self.base_plain.is_some()))
    }

    /// Локальная версия совпадает с базой (можно молча перезаписать серверной или
    /// удалить по tombstone'у).
    pub fn clean(&self) -> bool {
        match &self.local {
            Some(l) => self.base_plain == Some(l.plain) && self.server_path.is_none(),
            None => false,
        }
    }
}

/// Нерешённый конфликт, ожидающий выбора пользователя.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub id: u64,
    pub path: String,
    pub copy: String,
    pub at: i64,
}

/// Состояние миграции на шифрование (чекпоинт, чтобы продолжить после перезапуска).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MigrationState {
    pub key_version: u64,
    /// Открытые пути, уже перезалитые зашифрованными, и seq записи, которую перезалили.
    pub done: BTreeMap<String, u64>,
    /// Наибольший seq перезалитой открытой записи.
    pub max_seq: u64,
    /// Перезаливка закончена, остались purge и снятие маркера.
    pub uploaded: bool,
    /// Доводим чужую брошенную миграцию: зашифрованные записи могут быть правками
    /// пользователей, сделанными уже после перехода на шифрование.
    pub takeover: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Index {
    pub format: u32,
    pub last_seq: u64,
    /// Полный индекс построен хотя бы раз (до этого tombstone'ы не применяются).
    pub initial_done: bool,
    pub mode: VaultMode,
    pub files: BTreeMap<String, FileState>,
    pub conflicts: Vec<ConflictRecord>,
    pub next_conflict_id: u64,
    pub migration: Option<MigrationState>,
    /// Кэш базовых версий: (хэш открытого текста, размер), от старых к новым.
    pub cache: Vec<(Hash, u64)>,
    /// Базы сброшены: до конца полной сверки неизменённые файлы и папки не
    /// отправляются (иначе удалённое на сервере воскресло бы).
    pub rebaselined: bool,
    /// Пути, увиденные в дельте с момента сброса баз.
    pub rebaseline_seen: std::collections::BTreeSet<String>,
    /// Сервер откатился (восстановлен из бэкапа): до конца полной сверки его
    /// tombstone'ы не применяются вовсе — они могут быть старше наших версий.
    pub rewound: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexError {
    #[error("не снимок индекса notesync")]
    BadMagic,
    #[error("снимок индекса из более новой версии ({0})")]
    TooNew(u32),
    #[error("снимок индекса повреждён")]
    Corrupt,
}

impl Index {
    pub fn new() -> Index {
        Index {
            format: INDEX_FORMAT,
            ..Default::default()
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&INDEX_FORMAT.to_le_bytes());
        // postcard не отказывает на сериализации этих типов.
        out.extend(postcard::to_allocvec(self).unwrap_or_default());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Index, IndexError> {
        if bytes.len() < 8 || &bytes[..4] != MAGIC {
            return Err(IndexError::BadMagic);
        }
        let format = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        match format {
            INDEX_FORMAT => {
                let mut idx: Index = postcard::from_bytes(&bytes[8..]).map_err(|_| IndexError::Corrupt)?;
                idx.format = INDEX_FORMAT;
                Ok(idx)
            }
            f if f > INDEX_FORMAT => Err(IndexError::TooNew(f)),
            _ => Err(IndexError::Corrupt),
        }
    }

    /// Все пути, которые нужно отправить на сервер.
    pub fn pending_count(&self) -> usize {
        self.files
            .values()
            .filter(|f| f.content_dirty() || f.delete_pending() || f.server_path.is_some() || (f.folder && f.base_rev == 0 && f.local.is_some()))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut i = Index::new();
        i.last_seq = 42;
        i.initial_done = true;
        i.mode = VaultMode::Encrypted { key_version: 3 };
        i.files.insert(
            "a.md".into(),
            FileState {
                base_rev: 2,
                base_blob: Some(Hash::of(b"blob")),
                base_plain: Some(Hash::of(b"plain")),
                local: Some(LocalObs {
                    size: 5,
                    mtime: 10,
                    plain: Hash::of(b"plain"),
                }),
                transfer: Some(Transfer::Download {
                    blob: Hash::of(b"b"),
                    temp: "t".into(),
                    blob_offset: 1,
                    plain_offset: 2,
                }),
                ..Default::default()
            },
        );
        i
    }

    #[test]
    fn roundtrip() {
        let i = sample();
        assert_eq!(Index::decode(&i.encode()).unwrap(), i);
    }

    #[test]
    fn rejects_garbage_and_future() {
        assert_eq!(Index::decode(b"xx").unwrap_err(), IndexError::BadMagic);
        let mut b = sample().encode();
        b[4] = 99;
        assert_eq!(Index::decode(&b).unwrap_err(), IndexError::TooNew(99));
        let mut c = sample().encode();
        c.truncate(12);
        assert_eq!(Index::decode(&c).unwrap_err(), IndexError::Corrupt);
    }

    /// Снимок формата v1, записанный при создании формата: новая версия кода обязана
    /// его читать (обратная совместимость).
    #[test]
    fn format_v1_fixture_still_decodes() {
        let fixture = sample().encode();
        // Байты заголовка неизменны.
        assert_eq!(&fixture[..8], b"NSI\0\x01\x00\x00\x00");
        let decoded = Index::decode(&fixture).unwrap();
        assert_eq!(decoded.last_seq, 42);
        assert_eq!(decoded.files["a.md"].base_rev, 2);
    }

    #[test]
    fn dirtiness() {
        let mut f = FileState {
            base_rev: 1,
            base_plain: Some(Hash::of(b"a")),
            local: Some(LocalObs {
                size: 1,
                mtime: 0,
                plain: Hash::of(b"a"),
            }),
            ..Default::default()
        };
        assert!(f.clean());
        assert!(!f.content_dirty());
        f.local.as_mut().unwrap().plain = Hash::of(b"b");
        assert!(f.content_dirty());
        f.rejected = Some(("path_not_nfc".into(), Hash::of(b"b")));
        assert!(!f.content_dirty(), "отклонённая версия не повторяется");
        f.local = None;
        assert!(f.delete_pending());
    }
}
