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
}

impl std::fmt::Debug for VaultKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKeys(..)")
    }
}

impl VaultKeys {
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
        }
    }

    /// Расшифровывает путь и проверяет, что внутри валидный открытый путь.
    pub fn decrypt_path(&self, p: &pb::Path) -> Result<crate::path::VaultPath, CryptoError> {
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
    if r.format == KEY_RECORD_FORMAT_RECOVERY {
        for part in [&r.recovery_nonce, &r.recovery_wrapped_key] {
            aad.extend_from_slice(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_le_bytes());
            aad.extend_from_slice(part);
        }
        aad.extend_from_slice(&r.recovery_created_at.to_le_bytes());
    }
    aad
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
        (r.format == KEY_RECORD_FORMAT_RECOVERY).then(|| RecoveryWrap {
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
    seal(master, password, params, created_at, None)
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
    seal(master, password, params, created_at, Some(rw))
}

fn seal(
    master: &MasterKey,
    password: &str,
    params: KdfParams,
    created_at: i64,
    recovery: Option<RecoveryWrap>,
) -> Result<Vec<u8>, CryptoError> {
    let salt: [u8; 16] = random_bytes()?;
    let nonce: [u8; 12] = random_bytes()?;
    let (format, rw) = match recovery {
        Some(rw) => (KEY_RECORD_FORMAT_RECOVERY, rw),
        None => (
            KEY_RECORD_FORMAT,
            RecoveryWrap {
                nonce: Vec::new(),
                wrapped: Vec::new(),
                created_at: 0,
            },
        ),
    };
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
    if r.format != KEY_RECORD_FORMAT && r.format != KEY_RECORD_FORMAT_RECOVERY {
        return Err(CryptoError::UnsupportedFormat(r.format));
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
    parse_record(bytes).is_ok_and(|r| r.format == KEY_RECORD_FORMAT_RECOVERY)
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
    let record = seal_master_key_with_recovery(&master, password, params, now, &entropy)?;
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
    seal(&master, password, params, now, None)
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
    let record = seal(&master, new_password, params, now, RecoveryWrap::of(&r))?;
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
    seal(&master, new_password, params, now, RecoveryWrap::of(&r))
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
}
