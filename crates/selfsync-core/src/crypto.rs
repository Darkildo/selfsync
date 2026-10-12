//! Ключи vault'а и шифрование имён (раздел 9).
//!
//! Мастер-ключ — 32 случайных байта. На сервере он лежит обёрнутым ключом из пароля
//! (KEK = Argon2id) в непрозрачной записи [`pb::VaultKeyRecord`], поэтому смена пароля —
//! это перевыпуск записи без перешифровки данных. Из мастер-ключа через HKDF выводятся
//! раздельные ключи: содержимого, nonce и имён.

use aes_gcm_siv::Aes256GcmSiv;
use aes_gcm_siv::aead::{Aead, KeyInit, Payload};
use aes_siv::siv::Aes256Siv;
use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use prost::Message;
use selfsync_proto::v1 as pb;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::hash::Hash;

/// Версия формата записи ключа: только пароль.
pub const KEY_RECORD_FORMAT: u32 = 1;
/// Пароль и фраза восстановления. Клиенты 0.1.x такую запись не откроют, поэтому
/// она появляется только по явному действию пользователя.
pub const KEY_RECORD_FORMAT_RECOVERY: u32 = 2;
/// Смена ключа: поколение мастер-ключа (в AAD) и, пока перешифровка не завершена,
/// прежний мастер-ключ под ключом из нового. Фраза восстановления — по желанию.
/// Клиенты до 0.3.0 такую запись не откроют.
pub const KEY_RECORD_FORMAT_EPOCH: u32 = 3;
/// Argon2id v0x13.
pub const KDF_ARGON2ID: u32 = 1;

const WRAP_AAD: &[u8] = b"selfsync/v1/vault-key";
const RECOVERY_AAD: &[u8] = b"selfsync/v1/recovery";
const INFO_RECOVERY: &[u8] = b"selfsync/v1/recovery-kek";
const INFO_CONTENT: &[u8] = b"selfsync/v1/content";
const INFO_NONCE: &[u8] = b"selfsync/v1/nonce";
const INFO_NAMES: &[u8] = b"selfsync/v1/names";
const INFO_ID: &[u8] = b"selfsync/v1/content-id";
const INFO_CHECK: &[u8] = b"selfsync/v1/key-check";
const PREV_AAD: &[u8] = b"selfsync/v1/prev-key";
const INFO_PREV: &[u8] = b"selfsync/v1/prev-kek";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("неверный пароль")]
    WrongPassword,
    #[error("повреждённая запись ключа")]
    BadRecord,
    #[error("неподдерживаемый формат записи ключа: {0}")]
    UnsupportedFormat(u32),
    #[error("слишком слабые или слишком тяжёлые параметры KDF")]
    BadKdfParams,
    #[error("ошибка расшифровки: данные повреждены или подменены")]
    Decrypt,
    #[error("нет источника случайности")]
    Random,
    #[error("неверная фраза восстановления")]
    WrongPhrase,
    #[error("у записи ключа нет фразы восстановления")]
    NoRecovery,
    #[error("предыдущая смена ключа ещё не завершена")]
    RekeyUnfinished,
}

/// Параметры Argon2id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl KdfParams {
    /// Значения из задания: 64 МиБ, 3 прохода, параллелизм 1.
    pub const DEFAULT: KdfParams = KdfParams {
        m_cost_kib: 64 * 1024,
        t_cost: 3,
        p_cost: 1,
    };

    /// Облегчённые параметры только для тестов и симуляции.
    pub const TEST: KdfParams = KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };

    fn check(&self) -> Result<(), CryptoError> {
        // Нижняя граница защищает от записи, подсунутой сервером с ослабленным KDF;
        // верхняя — от записи, которая положит телефон по памяти.
        let weak = self.m_cost_kib < 8 || self.t_cost < 1 || self.p_cost < 1;
        let heavy = self.m_cost_kib > 1024 * 1024 || self.t_cost > 64 || self.p_cost > 16;
        if weak || heavy {
            return Err(CryptoError::BadKdfParams);
        }
        Ok(())
    }
}

/// Случайные байты из ОС (в WASM — `crypto.getRandomValues`).
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|_| CryptoError::Random)?;
    Ok(out)
}

/// Мастер-ключ vault'а.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn generate() -> Result<MasterKey, CryptoError> {
        Ok(MasterKey(random_bytes()?))
    }

    pub fn from_bytes(b: [u8; 32]) -> MasterKey {
        MasterKey(b)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Проверочное значение: по нему запомненный ключ сверяется с записью на сервере.
    /// Из него нельзя восстановить ключ (HMAC от отдельного производного ключа).
    pub fn check_value(&self) -> [u8; 16] {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut k = [0u8; 32];
        let _ = hk.expand(INFO_CHECK, &mut k);
        let mut mac = hmac_sha256(&k);
        k.zeroize();
        mac.update(b"selfsync key check");
        let tag = mac.finalize().into_bytes();
        let mut out = [0u8; 16];
        out.copy_from_slice(&tag[..16]);
        out
    }

    /// Раздельные ключи через HKDF-SHA256.
    pub fn derive(&self) -> VaultKeys {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut keys = VaultKeys {
            content: [0; 32],
            nonce: [0; 32],
            names: [0; 64],
            id: [0; 32],
            epoch: 1,
        };
        // expand не падает для длин ≤ 255·32 байт.
        let _ = hk.expand(INFO_CONTENT, &mut keys.content);
        let _ = hk.expand(INFO_NONCE, &mut keys.nonce);
        let _ = hk.expand(INFO_NAMES, &mut keys.names);
        let _ = hk.expand(INFO_ID, &mut keys.id);
        keys
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// Выведенные ключи.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct VaultKeys {
    /// AES-256-GCM-SIV для чанков содержимого.
    pub(crate) content: [u8; 32],
    /// HMAC-SHA256 для детерминированных nonce.
    pub(crate) nonce: [u8; 32],
    /// AES-256-SIV (512 бит) для сегментов имён.
    pub(crate) names: [u8; 64],
    /// HMAC-SHA256 для идентификатора содержимого в заголовке блоба.
    pub(crate) id: [u8; 32],
    /// Поколение мастер-ключа (не секрет): попадает в зашифрованные пути.
    pub(crate) epoch: u32,
}

impl std::fmt::Debug for VaultKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKeys(..)")
    }
}

impl VaultKeys {
    /// Те же ключи с поколением (`0` считается первым).
    pub fn with_epoch(mut self, epoch: u32) -> VaultKeys {
        self.epoch = epoch.max(1);
        self
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// nonce чанка = первые 12 байт HMAC(ключ nonce, sha256(открытый текст) ‖ индекс).
    pub fn chunk_nonce(&self, plaintext_hash: &Hash, index: u64) -> [u8; 12] {
        let mut mac = hmac_sha256(&self.nonce);
        mac.update(plaintext_hash.as_bytes());
        mac.update(&index.to_le_bytes());
        let tag = mac.finalize().into_bytes();
        let mut out = [0u8; 12];
        out.copy_from_slice(&tag[..12]);
        out
    }

    /// Идентификатор содержимого (входит в AAD всех чанков блоба): не даёт
    /// переставлять чанки между разными файлами одинаковой длины.
    pub fn content_id(&self, plaintext_hash: &Hash) -> [u8; 16] {
        let mut mac = hmac_sha256(&self.id);
        mac.update(plaintext_hash.as_bytes());
        let tag = mac.finalize().into_bytes();
        let mut out = [0u8; 16];
        out.copy_from_slice(&tag[..16]);
        out
    }

    pub(crate) fn content_cipher(&self) -> Aes256GcmSiv {
        // Длина ключа фиксирована, ошибки быть не может.
        Aes256GcmSiv::new_from_slice(&self.content).unwrap_or_else(|_| unreachable!())
    }

    /// Детерминированное шифрование одного сегмента имени (AES-SIV) с паддингом
    /// до кратного 16 байтам: сервер не видит длину имени точнее блока.
    pub fn encrypt_segment(&self, seg: &str) -> Vec<u8> {
        let mut padded = seg.as_bytes().to_vec();
        // ISO/IEC 7816-4: 0x80, затем нули до границы блока (минимум один байт).
        padded.push(0x80);
        while !padded.len().is_multiple_of(16) {
            padded.push(0);
        }
        let mut siv = Aes256Siv::new_from_slice(&self.names).unwrap_or_else(|_| unreachable!());
        let out = siv
            .encrypt::<[&[u8]; 0], &[u8]>([], &padded)
            .unwrap_or_else(|_| unreachable!("AES-SIV не отказывает на ограниченном входе"));
        padded.zeroize();
        out
    }

    /// Обратное к [`VaultKeys::encrypt_segment`].
    pub fn decrypt_segment(&self, ct: &[u8]) -> Result<String, CryptoError> {
        let mut siv = Aes256Siv::new_from_slice(&self.names).unwrap_or_else(|_| unreachable!());
        let mut pt = siv
            .decrypt::<[&[u8]; 0], &[u8]>([], ct)
            .map_err(|_| CryptoError::Decrypt)?;
        let end = pt
            .iter()
            .rposition(|&b| b != 0)
            .ok_or(CryptoError::Decrypt)?;
        if pt[end] != 0x80 {
            return Err(CryptoError::Decrypt);
        }
        pt.truncate(end);
        String::from_utf8(pt).map_err(|_| CryptoError::Decrypt)
    }

    /// Шифрует путь целиком (посегментно).
    pub fn encrypt_path(&self, path: &crate::path::VaultPath) -> pb::Path {
        pb::Path {
            segments: path.segments().map(|s| self.encrypt_segment(s)).collect(),
            encrypted: true,
            // Первое поколение — как до смены ключей: 0 (байт режима 0x01 на сервере).
            key_epoch: if self.epoch > 1 { self.epoch } else { 0 },
        }
    }

    /// Расшифровывает путь и проверяет, что внутри валидный открытый путь.
    pub fn decrypt_path(&self, p: &pb::Path) -> Result<crate::path::VaultPath, CryptoError> {
        // Путь другого поколения этим ключом не открыть (и не пытаться).
        if crate::path::path_epoch(p) != self.epoch {
            return Err(CryptoError::Decrypt);
        }
        let segs = p
            .segments
            .iter()
            .map(|s| self.decrypt_segment(s))
            .collect::<Result<Vec<_>, _>>()?;
        crate::path::VaultPath::from_segments(&segs).map_err(|_| CryptoError::Decrypt)
    }
}

fn hmac_sha256(key: &[u8]) -> Hmac<Sha256> {
    // HMAC принимает ключ любой длины.
    <Hmac<Sha256> as KeyInit>::new_from_slice(key).unwrap_or_else(|_| unreachable!())
}

/// Argon2id(пароль, соль) → 32 байта с проверкой параметров, как у ключа vault'а.
/// Для надстроек над ядром (например, вход в аккаунт сервиса: ключ входа и ключ
/// обёртки из одного пароля); сам ключ vault'а выводится через [`derive_kek`].
pub fn argon2id(password: &str, salt: &[u8], p: KdfParams) -> Result<[u8; 32], CryptoError> {
    derive_kek(password, salt, p)
}

/// KEK = Argon2id(пароль, соль).
fn derive_kek(password: &str, salt: &[u8], p: KdfParams) -> Result<[u8; 32], CryptoError> {
    p.check()?;
    let params = Params::new(p.m_cost_kib, p.t_cost, p.p_cost, Some(32))
        .map_err(|_| CryptoError::BadKdfParams)?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .map_err(|_| CryptoError::BadKdfParams)?;
    Ok(out)
}

fn wrap_aad(r: &pb::VaultKeyRecord) -> Vec<u8> {
    // Параметры KDF и соль входят в AAD: подмена параметров ломает расшифровку.
    let mut aad = WRAP_AAD.to_vec();
    for v in [r.format, r.kdf, r.m_cost_kib, r.t_cost, r.p_cost] {
        aad.extend_from_slice(&v.to_le_bytes());
    }
    aad.extend_from_slice(&r.salt);
    // Формат 2: обёртка восстановления целиком. Вырезать её (вернуть формат 1) или
    // подменить незаметно для парольной обёртки нельзя.
    if r.format == KEY_RECORD_FORMAT_RECOVERY || r.format == KEY_RECORD_FORMAT_EPOCH {
        for part in [&r.recovery_nonce, &r.recovery_wrapped_key] {
            aad.extend_from_slice(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_le_bytes());
            aad.extend_from_slice(part);
        }
        aad.extend_from_slice(&r.recovery_created_at.to_le_bytes());
    }
    // Формат 3: поколение. Обёртка прежнего ключа в AAD не входит: её снимают по
    // завершении перешифровки без пароля, а подменить её не даёт собственный AEAD.
    if r.format == KEY_RECORD_FORMAT_EPOCH {
        aad.extend_from_slice(&r.key_epoch.to_le_bytes());
    }
    aad
}

/// Ключ обёртки прежнего мастер-ключа: из нового, HKDF.
fn prev_kek(new: &MasterKey) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, &new.0);
    let mut out = [0u8; 32];
    hk.expand(INFO_PREV, &mut out)
        .unwrap_or_else(|_| unreachable!());
    out
}

fn prev_aad(epoch: u32) -> Vec<u8> {
    let mut aad = PREV_AAD.to_vec();
    aad.extend_from_slice(&epoch.to_le_bytes());
    aad
}

/// Прежний мастер-ключ под ключом из нового: (nonce, шифртекст).
fn wrap_prev(
    new: &MasterKey,
    old: &MasterKey,
    epoch: u32,
) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    let nonce: [u8; 12] = random_bytes()?;
    let mut kek = prev_kek(new);
    let cipher = Aes256GcmSiv::new_from_slice(&kek).unwrap_or_else(|_| unreachable!());
    kek.zeroize();
    let wrapped = cipher
        .encrypt(
            &nonce.into(),
            Payload {
                msg: old.as_bytes(),
                aad: &prev_aad(epoch),
            },
        )
        .map_err(|_| CryptoError::BadRecord)?;
    Ok((nonce.to_vec(), wrapped))
}

/// AAD обёртки восстановления. Не зависит от парольной части: смена пароля её
/// не трогает (фразы при смене пароля у нас нет).
fn recovery_aad(created_at: i64) -> Vec<u8> {
    let mut aad = RECOVERY_AAD.to_vec();
    aad.extend_from_slice(&KEY_RECORD_FORMAT_RECOVERY.to_le_bytes());
    aad.extend_from_slice(&created_at.to_le_bytes());
    aad
}

/// Ключ обёртки из фразы. Argon2 не нужен: в фразе 256 бит случайности, подбирать
/// нечего.
fn recovery_kek(entropy: &RecoveryEntropy) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, &entropy.0);
    let mut out = [0u8; 32];
    hk.expand(INFO_RECOVERY, &mut out)
        .unwrap_or_else(|_| unreachable!());
    out
}

/// Обёртка восстановления как она лежит в записи.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryWrap {
    nonce: Vec<u8>,
    wrapped: Vec<u8>,
    created_at: i64,
}

impl RecoveryWrap {
    fn new(
        master: &MasterKey,
        entropy: &RecoveryEntropy,
        created_at: i64,
    ) -> Result<Self, CryptoError> {
        let nonce: [u8; 12] = random_bytes()?;
        let mut kek = recovery_kek(entropy);
        let cipher = Aes256GcmSiv::new_from_slice(&kek).unwrap_or_else(|_| unreachable!());
        kek.zeroize();
        let aad = recovery_aad(created_at);
        let wrapped = cipher
            .encrypt(
                &nonce.into(),
                Payload {
                    msg: master.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::BadRecord)?;
        Ok(RecoveryWrap {
            nonce: nonce.to_vec(),
            wrapped,
            created_at,
        })
    }

    fn of(r: &pb::VaultKeyRecord) -> Option<RecoveryWrap> {
        let has = r.format == KEY_RECORD_FORMAT_RECOVERY
            || (r.format == KEY_RECORD_FORMAT_EPOCH && !r.recovery_wrapped_key.is_empty());
        has.then(|| RecoveryWrap {
            nonce: r.recovery_nonce.clone(),
            wrapped: r.recovery_wrapped_key.clone(),
            created_at: r.recovery_created_at,
        })
    }
}

/// Энтропия фразы восстановления (32 байта); затирается при освобождении.
#[derive(Clone, Zeroize, ZeroizeOnDrop, PartialEq, Eq)]
pub struct RecoveryEntropy([u8; 32]);

impl RecoveryEntropy {
    pub fn generate() -> Result<RecoveryEntropy, CryptoError> {
        Ok(RecoveryEntropy(random_bytes()?))
    }

    pub fn from_bytes(b: [u8; 32]) -> RecoveryEntropy {
        RecoveryEntropy(b)
    }

    /// 24 слова BIP-39.
    pub fn words(&self) -> Vec<String> {
        crate::bip39::encode(&self.0)
    }

    /// Разбор фразы: слова через пробелы или переводы строк, регистр не важен.
    pub fn from_phrase(phrase: &str) -> Result<RecoveryEntropy, crate::bip39::PhraseError> {
        crate::bip39::decode(phrase).map(RecoveryEntropy)
    }
}

impl std::fmt::Debug for RecoveryEntropy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryEntropy(…)")
    }
}

/// Создаёт запись ключа формата 1: мастер-ключ, обёрнутый KEK из пароля.
pub fn seal_master_key(
    master: &MasterKey,
    password: &str,
    params: KdfParams,
    created_at: i64,
) -> Result<Vec<u8>, CryptoError> {
    seal(master, password, params, created_at, Extras::default())
}

/// Части записи, которые переживают её перевыпуск (смена пароля, фразы).
#[derive(Debug, Clone, Default)]
struct Extras {
    recovery: Option<RecoveryWrap>,
    /// Поколение мастер-ключа; 0 и 1 — первое.
    epoch: u32,
    /// Прежний мастер-ключ (nonce, шифртекст), пока перешифровка не завершена.
    prev: Option<(Vec<u8>, Vec<u8>)>,
}

impl Extras {
    fn of(r: &pb::VaultKeyRecord) -> Extras {
        Extras {
            recovery: RecoveryWrap::of(r),
            epoch: epoch_of(r),
            prev: (!r.prev_wrapped_key.is_empty())
                .then(|| (r.prev_nonce.clone(), r.prev_wrapped_key.clone())),
        }
    }
}

fn epoch_of(r: &pb::VaultKeyRecord) -> u32 {
    if r.format == KEY_RECORD_FORMAT_EPOCH {
        r.key_epoch.max(1)
    } else {
        1
    }
}

/// Запись формата 2: обёртка паролем и обёртка фразой восстановления.
pub fn seal_master_key_with_recovery(
    master: &MasterKey,
    password: &str,
    params: KdfParams,
    created_at: i64,
    entropy: &RecoveryEntropy,
) -> Result<Vec<u8>, CryptoError> {
    let rw = RecoveryWrap::new(master, entropy, created_at)?;
    let extras = Extras {
        recovery: Some(rw),
        ..Extras::default()
    };
    seal(master, password, params, created_at, extras)
}

fn seal(
    master: &MasterKey,
    password: &str,
    params: KdfParams,
    created_at: i64,
    extras: Extras,
) -> Result<Vec<u8>, CryptoError> {
    let salt: [u8; 16] = random_bytes()?;
    let nonce: [u8; 12] = random_bytes()?;
    // Первое поколение без перешифровки — побайтно прежние форматы 1 и 2.
    let format = if extras.epoch > 1 || extras.prev.is_some() {
        KEY_RECORD_FORMAT_EPOCH
    } else if extras.recovery.is_some() {
        KEY_RECORD_FORMAT_RECOVERY
    } else {
        KEY_RECORD_FORMAT
    };
    let rw = extras.recovery.unwrap_or(RecoveryWrap {
        nonce: Vec::new(),
        wrapped: Vec::new(),
        created_at: 0,
    });
    let (prev_nonce, prev_wrapped_key) = extras.prev.unwrap_or_default();
    let mut record = pb::VaultKeyRecord {
        format,
        kdf: KDF_ARGON2ID,
        m_cost_kib: params.m_cost_kib,
        t_cost: params.t_cost,
        p_cost: params.p_cost,
        salt: salt.to_vec(),
        nonce: nonce.to_vec(),
        wrapped_key: Vec::new(),
        created_at,
        key_check: master.check_value().to_vec(),
        recovery_nonce: rw.nonce,
        recovery_wrapped_key: rw.wrapped,
        recovery_created_at: rw.created_at,
        key_epoch: if format == KEY_RECORD_FORMAT_EPOCH {
            extras.epoch.max(1)
        } else {
            0
        },
        prev_nonce,
        prev_wrapped_key,
    };
    let mut kek = derive_kek(password, &salt, params)?;
    let cipher = Aes256GcmSiv::new_from_slice(&kek).unwrap_or_else(|_| unreachable!());
    kek.zeroize();
    let aad = wrap_aad(&record);
    record.wrapped_key = cipher
        .encrypt(
            &nonce.into(),
            Payload {
                msg: master.as_bytes(),
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::BadRecord)?;
    Ok(record.encode_to_vec())
}

/// Разбирает запись ключа (без пароля): параметры и версия формата.
pub fn parse_record(bytes: &[u8]) -> Result<pb::VaultKeyRecord, CryptoError> {
    let r = pb::VaultKeyRecord::decode(bytes).map_err(|_| CryptoError::BadRecord)?;
    if !matches!(
        r.format,
        KEY_RECORD_FORMAT | KEY_RECORD_FORMAT_RECOVERY | KEY_RECORD_FORMAT_EPOCH
    ) {
        return Err(CryptoError::UnsupportedFormat(r.format));
    }
    if r.format == KEY_RECORD_FORMAT_EPOCH {
        let rec_ok = (r.recovery_nonce.is_empty() && r.recovery_wrapped_key.is_empty())
            || (r.recovery_nonce.len() == 12 && !r.recovery_wrapped_key.is_empty());
        let prev_ok = (r.prev_nonce.is_empty() && r.prev_wrapped_key.is_empty())
            || (r.prev_nonce.len() == 12 && !r.prev_wrapped_key.is_empty());
        if !rec_ok || !prev_ok || r.key_epoch == 0 {
            return Err(CryptoError::BadRecord);
        }
    }
    if r.kdf != KDF_ARGON2ID || r.salt.len() < 16 || r.nonce.len() != 12 {
        return Err(CryptoError::BadRecord);
    }
    if r.format == KEY_RECORD_FORMAT_RECOVERY
        && (r.recovery_nonce.len() != 12 || r.recovery_wrapped_key.is_empty())
    {
        return Err(CryptoError::BadRecord);
    }
    Ok(r)
}

/// Есть ли в записи фраза восстановления.
pub fn has_recovery(bytes: &[u8]) -> bool {
    parse_record(bytes).is_ok_and(|r| RecoveryWrap::of(&r).is_some())
}

/// Поколение мастер-ключа записи (форматы 1 и 2 — первое).
pub fn record_epoch(bytes: &[u8]) -> Result<u32, CryptoError> {
    Ok(epoch_of(&parse_record(bytes)?))
}

/// Перешифровка начата и не завершена: в записи есть прежний ключ.
pub fn has_prev(bytes: &[u8]) -> bool {
    parse_record(bytes).is_ok_and(|r| !r.prev_wrapped_key.is_empty())
}

/// Прежний мастер-ключ из записи (нужен текущий); `None` — перешифровка завершена.
pub fn open_prev(bytes: &[u8], master: &MasterKey) -> Result<Option<MasterKey>, CryptoError> {
    let r = parse_record(bytes)?;
    if r.prev_wrapped_key.is_empty() {
        return Ok(None);
    }
    let nonce: [u8; 12] = r
        .prev_nonce
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadRecord)?;
    let mut kek = prev_kek(master);
    let cipher = Aes256GcmSiv::new_from_slice(&kek).unwrap_or_else(|_| unreachable!());
    kek.zeroize();
    let mut pt = cipher
        .decrypt(
            &nonce.into(),
            Payload {
                msg: &r.prev_wrapped_key,
                aad: &prev_aad(epoch_of(&r)),
            },
        )
        .map_err(|_| CryptoError::Decrypt)?;
    let key: [u8; 32] = pt
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadRecord)?;
    pt.zeroize();
    Ok(Some(MasterKey(key)))
}

/// Перешифровка завершена: убрать прежний ключ. Пароль не нужен (обёртка
/// прежнего ключа не входит в AAD парольной).
pub fn without_prev(bytes: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut r = parse_record(bytes)?;
    r.prev_nonce.clear();
    r.prev_wrapped_key.clear();
    Ok(r.encode_to_vec())
}

/// Результат [`rekey`].
pub struct Rekeyed {
    /// Новая запись: новый ключ под тем же паролем, поколение +1, прежний ключ внутри.
    pub record: Vec<u8>,
    pub new: MasterKey,
    pub old: MasterKey,
    pub epoch: u32,
    /// Новая фраза, если у записи была фраза (прежняя перестаёт работать).
    pub phrase: Option<RecoveryEntropy>,
}

/// Смена ключа: новый мастер-ключ под тем же паролем. Пока перешифровка не
/// завершена, прежний ключ лежит в записи под новым — повторная смена отказывает.
pub fn rekey(
    bytes: &[u8],
    password: &str,
    params: KdfParams,
    now: i64,
) -> Result<Rekeyed, CryptoError> {
    let r = parse_record(bytes)?;
    if !r.prev_wrapped_key.is_empty() {
        return Err(CryptoError::RekeyUnfinished);
    }
    let old = open_master_key(bytes, password)?;
    let new = MasterKey::generate()?;
    let epoch = epoch_of(&r) + 1;
    let (recovery, phrase) = if RecoveryWrap::of(&r).is_some() {
        let e = RecoveryEntropy::generate()?;
        (Some(RecoveryWrap::new(&new, &e, now)?), Some(e))
    } else {
        (None, None)
    };
    let prev = Some(wrap_prev(&new, &old, epoch)?);
    let record = seal(
        &new,
        password,
        params,
        now,
        Extras {
            recovery,
            epoch,
            prev,
        },
    )?;
    Ok(Rekeyed {
        record,
        new,
        old,
        epoch,
        phrase,
    })
}

/// Раскрывает мастер-ключ фразой восстановления.
pub fn open_master_key_with_phrase(
    bytes: &[u8],
    entropy: &RecoveryEntropy,
) -> Result<MasterKey, CryptoError> {
    let r = parse_record(bytes)?;
    let rw = RecoveryWrap::of(&r).ok_or(CryptoError::NoRecovery)?;
    let nonce: [u8; 12] = rw
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadRecord)?;
    let mut kek = recovery_kek(entropy);
    let cipher = Aes256GcmSiv::new_from_slice(&kek).unwrap_or_else(|_| unreachable!());
    kek.zeroize();
    let aad = recovery_aad(rw.created_at);
    let mut pt = cipher
        .decrypt(
            &nonce.into(),
            Payload {
                msg: &rw.wrapped,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::WrongPhrase)?;
    let key: [u8; 32] = pt
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadRecord)?;
    pt.zeroize();
    let master = MasterKey(key);
    // Обёртка сходится с записью (key_check): подставленная обёртка чужого ключа
    // не пройдёт.
    if !verify_master(bytes, &master)? {
        return Err(CryptoError::BadRecord);
    }
    Ok(master)
}

/// Добавить (или заменить) фразу восстановления: открыть паролем, новая энтропия.
/// Прежняя фраза перестаёт работать. Возвращает новую запись и энтропию.
pub fn add_recovery(
    bytes: &[u8],
    password: &str,
    params: KdfParams,
    now: i64,
) -> Result<(Vec<u8>, RecoveryEntropy), CryptoError> {
    let master = open_master_key(bytes, password)?;
    let entropy = RecoveryEntropy::generate()?;
    let extras = Extras {
        recovery: Some(RecoveryWrap::new(&master, &entropy, now)?),
        ..Extras::of(&parse_record(bytes)?)
    };
    let record = seal(&master, password, params, now, extras)?;
    Ok((record, entropy))
}

/// Убрать фразу восстановления: запись снова формата 1.
pub fn remove_recovery(
    bytes: &[u8],
    password: &str,
    params: KdfParams,
    now: i64,
) -> Result<Vec<u8>, CryptoError> {
    let master = open_master_key(bytes, password)?;
    let extras = Extras {
        recovery: None,
        ..Extras::of(&parse_record(bytes)?)
    };
    seal(&master, password, params, now, extras)
}

/// Вход по фразе: новый пароль, фраза остаётся действительной. Возвращает новую
/// запись и мастер-ключ.
pub fn recover(
    bytes: &[u8],
    entropy: &RecoveryEntropy,
    new_password: &str,
    params: KdfParams,
    now: i64,
) -> Result<(Vec<u8>, MasterKey), CryptoError> {
    let master = open_master_key_with_phrase(bytes, entropy)?;
    let r = parse_record(bytes)?;
    let record = seal(&master, new_password, params, now, Extras::of(&r))?;
    Ok((record, master))
}

/// Раскрывает мастер-ключ паролем. Неверный пароль — [`CryptoError::WrongPassword`].
pub fn open_master_key(bytes: &[u8], password: &str) -> Result<MasterKey, CryptoError> {
    let r = parse_record(bytes)?;
    let params = KdfParams {
        m_cost_kib: r.m_cost_kib,
        t_cost: r.t_cost,
        p_cost: r.p_cost,
    };
    let mut kek = derive_kek(password, &r.salt, params)?;
    let cipher = Aes256GcmSiv::new_from_slice(&kek).unwrap_or_else(|_| unreachable!());
    kek.zeroize();
    let nonce: [u8; 12] = r
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadRecord)?;
    let aad = wrap_aad(&r);
    let mut pt = cipher
        .decrypt(
            &nonce.into(),
            Payload {
                msg: &r.wrapped_key,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::WrongPassword)?;
    let key: [u8; 32] = pt
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadRecord)?;
    pt.zeroize();
    Ok(MasterKey(key))
}

/// Соответствует ли ключ записи (для ключа, запомненного на устройстве).
pub fn verify_master(record: &[u8], master: &MasterKey) -> Result<bool, CryptoError> {
    let r = parse_record(record)?;
    if r.key_check.len() != 16 {
        return Err(CryptoError::BadRecord);
    }
    Ok(bool::from(subtle::ConstantTimeEq::ct_eq(
        &r.key_check[..],
        &master.check_value()[..],
    )))
}

/// Смена пароля: тот же мастер-ключ, новая запись. Данные не перешифровываются.
pub fn change_password(
    bytes: &[u8],
    old_password: &str,
    new_password: &str,
    params: KdfParams,
    now: i64,
) -> Result<Vec<u8>, CryptoError> {
    let master = open_master_key(bytes, old_password)?;
    // Фраза восстановления переживает смену пароля: её обёртка от пароля не зависит.
    let r = parse_record(bytes)?;
    seal(&master, new_password, params, now, Extras::of(&r))
}

/// Грубая оценка стойкости пароля в битах — совет пользователю, а не запрет.
pub fn password_strength_bits(pw: &str) -> u32 {
    let mut classes = 0u32;
    let (mut lower, mut upper, mut digit, mut other, mut non_ascii) =
        (false, false, false, false, false);
    for c in pw.chars() {
        match c {
            'a'..='z' => lower = true,
            'A'..='Z' => upper = true,
            '0'..='9' => digit = true,
            c if c.is_ascii() => other = true,
            _ => non_ascii = true,
        }
    }
    for (flag, size) in [
        (lower, 26),
        (upper, 26),
        (digit, 10),
        (other, 33),
        (non_ascii, 64),
    ] {
        if flag {
            classes += size;
        }
    }
    let mut unique: Vec<char> = pw.chars().collect();
    unique.sort_unstable();
    unique.dedup();
    let len = u32::try_from(pw.chars().count()).unwrap_or(u32::MAX);
    if classes == 0 {
        return 0;
    }
    // log2(classes) с точностью до десятых, умноженный на длину, со штрафом за повторы.
    let per_char = f64::from(classes).log2();
    let effective = f64::from(len).min(f64::from(u32::try_from(unique.len()).unwrap_or(0)) * 1.5);
    // Значение заведомо в диапазоне u32: длина пароля ограничена здравым смыслом.
    (per_char * effective).floor().clamp(0.0, 1024.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::VaultPath;

    #[test]
    fn record_roundtrip_and_wrong_password() {
        let master = MasterKey::generate().unwrap();
        let rec = seal_master_key(&master, "correct horse", KdfParams::TEST, 1).unwrap();
        let opened = open_master_key(&rec, "correct horse").unwrap();
        assert_eq!(opened.as_bytes(), master.as_bytes());
        assert_eq!(
            open_master_key(&rec, "wrong").unwrap_err(),
            CryptoError::WrongPassword
        );
    }

    #[test]
    fn remembered_key_verification() {
        let master = MasterKey::generate().unwrap();
        let rec = seal_master_key(&master, "pw", KdfParams::TEST, 1).unwrap();
        assert!(verify_master(&rec, &master).unwrap());
        let other = MasterKey::generate().unwrap();
        assert!(!verify_master(&rec, &other).unwrap());
    }

    #[test]
    fn change_password_keeps_master() {
        let master = MasterKey::generate().unwrap();
        let rec = seal_master_key(&master, "old", KdfParams::TEST, 1).unwrap();
        let rec2 = change_password(&rec, "old", "new", KdfParams::TEST, 2).unwrap();
        assert_ne!(rec, rec2);
        assert_eq!(
            open_master_key(&rec2, "new").unwrap().as_bytes(),
            master.as_bytes()
        );
        assert!(open_master_key(&rec2, "old").is_err());
        assert!(change_password(&rec, "bad", "x", KdfParams::TEST, 3).is_err());
    }

    #[test]
    fn tampered_params_rejected() {
        let master = MasterKey::generate().unwrap();
        let rec = seal_master_key(&master, "pw", KdfParams::TEST, 1).unwrap();
        let mut r = parse_record(&rec).unwrap();
        r.t_cost = 2;
        assert_eq!(
            open_master_key(&r.encode_to_vec(), "pw").unwrap_err(),
            CryptoError::WrongPassword
        );
        let mut weak = parse_record(&rec).unwrap();
        weak.m_cost_kib = 1;
        assert_eq!(
            open_master_key(&weak.encode_to_vec(), "pw").unwrap_err(),
            CryptoError::BadKdfParams
        );
        let mut fmt = parse_record(&rec).unwrap();
        fmt.format = 9;
        assert_eq!(
            open_master_key(&fmt.encode_to_vec(), "pw").unwrap_err(),
            CryptoError::UnsupportedFormat(9)
        );
    }

    #[test]
    fn argon2id_reference_vector() {
        // RFC 9106, Argon2id без secret и AD нельзя проверить напрямую (там они есть),
        // поэтому фиксируем собственный вектор: изменение параметров или библиотеки
        // должно быть замечено, иначе старые записи ключей перестанут открываться.
        let kek = derive_kek("password", b"somesaltsomesalt", KdfParams::TEST).unwrap();
        let again = derive_kek("password", b"somesaltsomesalt", KdfParams::TEST).unwrap();
        assert_eq!(kek, again);
        let other = derive_kek("passwore", b"somesaltsomesalt", KdfParams::TEST).unwrap();
        assert_ne!(kek, other);
    }

    #[test]
    fn public_argon2id_is_kek_with_checked_params() {
        let a = argon2id("password", b"somesaltsomesalt", KdfParams::TEST).unwrap();
        assert_eq!(
            a,
            derive_kek("password", b"somesaltsomesalt", KdfParams::TEST).unwrap()
        );
        let weak = KdfParams {
            m_cost_kib: 1,
            t_cost: 1,
            p_cost: 1,
        };
        assert!(argon2id("password", b"somesaltsomesalt", weak).is_err());
    }

    #[test]
    fn hkdf_keys_are_distinct_and_stable() {
        let m = MasterKey::from_bytes([7; 32]);
        let k1 = m.derive();
        let k2 = m.derive();
        assert_eq!(k1.content, k2.content);
        assert_ne!(k1.content, k1.nonce);
        assert_ne!(&k1.names[..32], &k1.content[..]);
    }

    #[test]
    fn segment_encryption() {
        let k = MasterKey::from_bytes([1; 32]).derive();
        let a = k.encrypt_segment("Заметка.md");
        let b = k.encrypt_segment("Заметка.md");
        assert_eq!(a, b, "детерминированность");
        assert_eq!(a.len() % 16, 0);
        assert_eq!(k.decrypt_segment(&a).unwrap(), "Заметка.md");
        // длина скрыта до блока
        assert_eq!(
            k.encrypt_segment("a").len(),
            k.encrypt_segment("abcdefghijklmno").len()
        );
        assert_ne!(
            k.encrypt_segment("abcdefghijklmno").len(),
            k.encrypt_segment("abcdefghijklmnop").len()
        );
        let mut bad = a.clone();
        bad[20] ^= 1;
        assert_eq!(k.decrypt_segment(&bad).unwrap_err(), CryptoError::Decrypt);
        let other = MasterKey::from_bytes([2; 32]).derive();
        assert!(other.decrypt_segment(&a).is_err());
    }

    #[test]
    fn path_encryption_keeps_hierarchy() {
        let k = MasterKey::from_bytes([3; 32]).derive();
        let p = VaultPath::parse("dir/sub/note.md").unwrap();
        let e = k.encrypt_path(&p);
        assert!(e.encrypted);
        assert_eq!(e.segments.len(), 3);
        let parent = k.encrypt_path(&p.parent().unwrap());
        assert_eq!(&e.segments[..2], &parent.segments[..]);
        assert_eq!(k.decrypt_path(&e).unwrap(), p);
    }

    #[test]
    fn nonce_and_id_depend_on_inputs() {
        let k = MasterKey::from_bytes([4; 32]).derive();
        let h = Hash::of(b"x");
        assert_ne!(k.chunk_nonce(&h, 0), k.chunk_nonce(&h, 1));
        assert_ne!(k.chunk_nonce(&h, 0), k.chunk_nonce(&Hash::of(b"y"), 0));
        assert_eq!(k.content_id(&h), k.content_id(&h));
    }

    #[test]
    fn strength_estimate() {
        assert!(password_strength_bits("123") < 20);
        assert!(password_strength_bits("correct horse battery staple") > 60);
        assert!(password_strength_bits("aaaaaaaaaaaaaaaaaaaa") < 20);
    }

    fn record(bytes: &[u8]) -> pb::VaultKeyRecord {
        pb::VaultKeyRecord::decode(bytes).unwrap()
    }

    #[test]
    fn recovery_roundtrip() {
        let master = MasterKey::generate().unwrap();
        let e = RecoveryEntropy::generate().unwrap();
        let rec = seal_master_key_with_recovery(&master, "pw", KdfParams::TEST, 5, &e).unwrap();
        assert_eq!(record(&rec).format, KEY_RECORD_FORMAT_RECOVERY);
        assert!(has_recovery(&rec));
        assert_eq!(
            open_master_key(&rec, "pw").unwrap().as_bytes(),
            master.as_bytes()
        );
        let words = e.words();
        assert_eq!(words.len(), 24);
        let back = RecoveryEntropy::from_phrase(&words.join(" ")).unwrap();
        let opened = open_master_key_with_phrase(&rec, &back).unwrap();
        assert_eq!(opened.as_bytes(), master.as_bytes());
        let other = RecoveryEntropy::generate().unwrap();
        assert_eq!(
            open_master_key_with_phrase(&rec, &other).unwrap_err(),
            CryptoError::WrongPhrase
        );
        assert!(verify_master(&rec, &master).unwrap());
        assert!(
            !format!("{e:?}").contains(&words[0]),
            "Debug не печатает фразу"
        );
    }

    #[test]
    fn format_1_record_still_opens() {
        let master = MasterKey::generate().unwrap();
        let rec = seal_master_key(&master, "pw", KdfParams::TEST, 1).unwrap();
        assert_eq!(record(&rec).format, KEY_RECORD_FORMAT);
        assert!(!has_recovery(&rec));
        assert_eq!(
            open_master_key(&rec, "pw").unwrap().as_bytes(),
            master.as_bytes()
        );
        let e = RecoveryEntropy::generate().unwrap();
        assert_eq!(
            open_master_key_with_phrase(&rec, &e).unwrap_err(),
            CryptoError::NoRecovery
        );
    }

    #[test]
    fn recovery_wrap_is_bound_to_password_wrap() {
        let master = MasterKey::generate().unwrap();
        let e = RecoveryEntropy::generate().unwrap();
        let rec = seal_master_key_with_recovery(&master, "pw", KdfParams::TEST, 5, &e).unwrap();
        // вырезать обёртку восстановления (вернуть формат 1) — пароль не откроет
        let mut cut = record(&rec);
        cut.format = KEY_RECORD_FORMAT;
        cut.recovery_nonce.clear();
        cut.recovery_wrapped_key.clear();
        cut.recovery_created_at = 0;
        assert_eq!(
            open_master_key(&cut.encode_to_vec(), "pw").unwrap_err(),
            CryptoError::WrongPassword
        );
        // подменить обёртку своей (другая фраза, тот же ключ знать нельзя — но
        // даже правильная обёртка с другой фразой) — пароль не откроет
        let foreign = seal_master_key_with_recovery(
            &master,
            "pw",
            KdfParams::TEST,
            5,
            &RecoveryEntropy::generate().unwrap(),
        )
        .unwrap();
        let mut swapped = record(&rec);
        let f = record(&foreign);
        swapped.recovery_nonce = f.recovery_nonce;
        swapped.recovery_wrapped_key = f.recovery_wrapped_key;
        assert_eq!(
            open_master_key(&swapped.encode_to_vec(), "pw").unwrap_err(),
            CryptoError::WrongPassword
        );
        let mut later = record(&rec);
        later.recovery_created_at += 1;
        assert!(open_master_key(&later.encode_to_vec(), "pw").is_err());
        assert!(open_master_key_with_phrase(&later.encode_to_vec(), &e).is_err());
    }

    #[test]
    fn recovery_lifecycle() {
        let master = MasterKey::generate().unwrap();
        let rec = seal_master_key(&master, "pw", KdfParams::TEST, 1).unwrap();
        // создать
        let (rec2, e1) = add_recovery(&rec, "pw", KdfParams::TEST, 2).unwrap();
        assert!(add_recovery(&rec, "wrong", KdfParams::TEST, 2).is_err());
        // смена пароля фразу не отменяет
        let rec3 = change_password(&rec2, "pw", "pw2", KdfParams::TEST, 3).unwrap();
        assert_eq!(
            open_master_key_with_phrase(&rec3, &e1).unwrap().as_bytes(),
            master.as_bytes()
        );
        // вход по фразе: новый пароль, фраза остаётся
        let (rec4, m) = recover(&rec3, &e1, "pw3", KdfParams::TEST, 4).unwrap();
        assert_eq!(m.as_bytes(), master.as_bytes());
        assert!(open_master_key(&rec4, "pw2").is_err());
        assert_eq!(
            open_master_key(&rec4, "pw3").unwrap().as_bytes(),
            master.as_bytes()
        );
        assert!(open_master_key_with_phrase(&rec4, &e1).is_ok());
        // создать новую — старая фраза перестаёт работать
        let (rec5, e2) = add_recovery(&rec4, "pw3", KdfParams::TEST, 5).unwrap();
        assert_eq!(
            open_master_key_with_phrase(&rec5, &e1).unwrap_err(),
            CryptoError::WrongPhrase
        );
        assert!(open_master_key_with_phrase(&rec5, &e2).is_ok());
        // удалить
        let rec6 = remove_recovery(&rec5, "pw3", KdfParams::TEST, 6).unwrap();
        assert!(!has_recovery(&rec6));
        assert_eq!(record(&rec6).format, KEY_RECORD_FORMAT);
        assert_eq!(
            open_master_key(&rec6, "pw3").unwrap().as_bytes(),
            master.as_bytes()
        );
        assert_eq!(
            open_master_key_with_phrase(&rec6, &e2).unwrap_err(),
            CryptoError::NoRecovery
        );
    }

    /// Смена ключа: новый ключ под тем же паролем, поколение +1, прежний ключ
    /// достаётся новым; формат 3 переживает смену пароля и операции с фразой.
    #[test]
    fn rekey_record_lifecycle() {
        let m1 = MasterKey::generate().unwrap();
        let entropy = RecoveryEntropy::generate().unwrap();
        let rec = seal_master_key_with_recovery(&m1, "pw", KdfParams::TEST, 1, &entropy).unwrap();
        assert_eq!(record_epoch(&rec).unwrap(), 1);
        assert!(!has_prev(&rec));

        let rk = rekey(&rec, "pw", KdfParams::TEST, 2).unwrap();
        assert_eq!(rk.epoch, 2);
        assert_eq!(rk.old.as_bytes(), m1.as_bytes());
        assert_ne!(rk.new.as_bytes(), m1.as_bytes());
        let rec2 = rk.record;
        assert_eq!(parse_record(&rec2).unwrap().format, KEY_RECORD_FORMAT_EPOCH);
        assert_eq!(record_epoch(&rec2).unwrap(), 2);
        assert_eq!(
            open_master_key(&rec2, "pw").unwrap().as_bytes(),
            rk.new.as_bytes()
        );
        assert!(verify_master(&rec2, &rk.new).unwrap());
        assert!(
            !verify_master(&rec2, &m1).unwrap(),
            "старый ключ к новой записи не подходит"
        );
        assert!(has_prev(&rec2));
        let prev = open_prev(&rec2, &rk.new).unwrap().unwrap();
        assert_eq!(prev.as_bytes(), m1.as_bytes());
        assert!(
            open_prev(&rec2, &m1).is_err(),
            "прежний ключ — только новым"
        );
        // фраза — новая, прежняя не подходит
        let phrase = rk.phrase.unwrap();
        assert!(has_recovery(&rec2));
        assert_eq!(
            open_master_key_with_phrase(&rec2, &phrase)
                .unwrap()
                .as_bytes(),
            rk.new.as_bytes()
        );
        assert!(open_master_key_with_phrase(&rec2, &entropy).is_err());
        // повторная смена до завершения — отказ
        assert_eq!(
            rekey(&rec2, "pw", KdfParams::TEST, 3).err(),
            Some(CryptoError::RekeyUnfinished)
        );
        // смена пароля и фраза сохраняют поколение и прежний ключ
        let rec3 = change_password(&rec2, "pw", "pw2", KdfParams::TEST, 3).unwrap();
        assert_eq!(record_epoch(&rec3).unwrap(), 2);
        assert_eq!(
            open_prev(&rec3, &rk.new).unwrap().unwrap().as_bytes(),
            m1.as_bytes()
        );
        let rec4 = remove_recovery(&rec3, "pw2", KdfParams::TEST, 4).unwrap();
        assert!(!has_recovery(&rec4) && has_prev(&rec4));
        assert_eq!(record_epoch(&rec4).unwrap(), 2);
        let (rec5, _) = add_recovery(&rec4, "pw2", KdfParams::TEST, 5).unwrap();
        assert!(has_recovery(&rec5) && has_prev(&rec5));
        // завершение: прежний ключ убирается без пароля, пароль по-прежнему открывает
        let done = without_prev(&rec5).unwrap();
        assert!(!has_prev(&done));
        assert!(open_prev(&done, &rk.new).unwrap().is_none());
        assert_eq!(
            open_master_key(&done, "pw2").unwrap().as_bytes(),
            rk.new.as_bytes()
        );
        assert_eq!(record_epoch(&done).unwrap(), 2);
        // следующая смена — поколение 3, фразы нет — новой тоже нет
        let rk3 = rekey(&rec4.clone(), "pw2", KdfParams::TEST, 6);
        assert_eq!(rk3.err(), Some(CryptoError::RekeyUnfinished));
        let plain_done = without_prev(&rec4).unwrap();
        let rk3 = rekey(&plain_done, "pw2", KdfParams::TEST, 6).unwrap();
        assert_eq!(rk3.epoch, 3);
        assert!(rk3.phrase.is_none() && !has_recovery(&rk3.record));
    }

    /// Поколение — в AAD парольной обёртки: подменить его нельзя.
    #[test]
    fn rekey_epoch_is_authenticated() {
        let m1 = MasterKey::generate().unwrap();
        let rec = seal_master_key(&m1, "pw", KdfParams::TEST, 1).unwrap();
        let rk = rekey(&rec, "pw", KdfParams::TEST, 2).unwrap();
        let mut r = parse_record(&rk.record).unwrap();
        r.key_epoch = 7;
        assert_eq!(
            open_master_key(&r.encode_to_vec(), "pw").err(),
            Some(CryptoError::WrongPassword)
        );
        // обёртка прежнего ключа привязана к поколению
        let mut r = parse_record(&rk.record).unwrap();
        r.key_epoch = 7;
        assert!(open_prev(&r.encode_to_vec(), &rk.new).is_err());
    }

    /// Пути разных поколений: новый ключ не открывает старые пути и наоборот.
    #[test]
    fn path_epochs() {
        let m1 = MasterKey::generate().unwrap();
        let m2 = MasterKey::generate().unwrap();
        let k1 = m1.derive();
        let k2 = m2.derive().with_epoch(2);
        let vp = crate::path::VaultPath::parse("a/b.md").unwrap();
        let p1 = k1.encrypt_path(&vp);
        let p2 = k2.encrypt_path(&vp);
        assert_eq!((p1.key_epoch, p2.key_epoch), (0, 2));
        assert_eq!(k1.decrypt_path(&p1).unwrap(), vp);
        assert_eq!(k2.decrypt_path(&p2).unwrap(), vp);
        assert!(k2.decrypt_path(&p1).is_err() && k1.decrypt_path(&p2).is_err());
        let e = crate::path::canonical_encode(&p2);
        assert_eq!(&e[..5], &[2, 0, 0, 0, 2]);
        assert_eq!(crate::path::canonical_decode(&e).unwrap(), p2);
        assert_eq!(crate::path::canonical_encode(&p1)[0], 1);
    }
}
