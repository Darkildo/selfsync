//! Передача ключа по коду подключения (протокол 1.4): мастер-ключ vault'а уходит
//! новому устройству обменом ECDH P-256 мимо сервера. Сервер хранит только открытые
//! ключи и обёртку и прочитать ключ не может. Подменить открытые ключи незаметно он
//! не может при входе по QR (`h` сверяет обязательство источника, MAC из `m` —
//! ключ нового устройства) и может только с вероятностью 10⁻⁶ на попытку при коде,
//! введённом руками, — если пользователь сверил цифры на обоих устройствах.
//!
//! Роли: **источник** (устройство с ключом, пара `a/A`) и **новое устройство**
//! (пара `b/B`). Порядок шагов — в `spec/protocol.md`; здесь только примитивы:
//!
//! - `commit = sha256(A)` — источник обязуется своим ключом до того, как увидит `B`;
//! - `h` — первые 16 байт `commit` в QR: новое устройство сверяет обязательство,
//!   полученное от сервера;
//! - `mac = HMAC-SHA256(m, B)` — `m` (16 байт) есть только в QR, сервер его не видит;
//! - цифры: `sha256(<метка> sas ‖ A ‖ B)`, первые 4 байта big-endian по модулю 10⁶;
//! - `k = HKDF-SHA256(ECDH(a, B), salt = A ‖ B, info = <метка> key)`;
//! - `wrapped = nonce ‖ AES-256-GCM(k, ключ)`, nonce — 12 случайных байт, без AAD.
//!
//! Метка домена ([`Labels`]) разводит selfsync и надстройки (облако берёт тот же
//! модуль с меткой `selfsync-cloud/handoff/v1`). Всё совместимо с WebCrypto: открытый
//! ключ — несжатая точка SEC1 (65 байт), общий секрет — координата x (32 байта).

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::{PublicKey, SecretKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::crypto::{CryptoError, random_bytes};

/// Длина открытого ключа: несжатая точка P-256 (`0x04 ‖ x ‖ y`).
pub const PUBLIC_LEN: usize = 65;
/// Длина секрета из QR.
pub const LINK_SECRET_LEN: usize = 16;
/// Длина nonce в обёртке.
pub const NONCE_LEN: usize = 12;
/// Метка selfsync (обмен при подключении по коду).
pub const SELFSYNC_PREFIX: &str = "selfsync/join-handoff/v1";
/// Метка облака (код аккаунта, `/account/handoff`).
pub const CLOUD_PREFIX: &str = "selfsync-cloud/handoff/v1";

/// Метки домена для цифр и ключа обёртки: `<префикс> sas` и `<префикс> key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    sas: Vec<u8>,
    key: Vec<u8>,
}

impl Labels {
    pub fn new(prefix: &str) -> Labels {
        Labels {
            sas: format!("{prefix} sas").into_bytes(),
            key: format!("{prefix} key").into_bytes(),
        }
    }

    pub fn selfsync() -> Labels {
        Labels::new(SELFSYNC_PREFIX)
    }
}

/// Одноразовая пара ключей обмена. Секрет стирается из памяти при удалении.
#[derive(Clone)]
pub struct Ephemeral {
    secret: SecretKey,
    public: [u8; PUBLIC_LEN],
}

impl std::fmt::Debug for Ephemeral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ephemeral(секрет скрыт)")
    }
}

impl Ephemeral {
    pub fn generate() -> Result<Ephemeral, CryptoError> {
        // Скаляр вне [1, n) выпадает с вероятностью ~2⁻³²: просто взять другой.
        loop {
            let b = Zeroizing::new(random_bytes::<32>()?);
            if let Ok(e) = Ephemeral::from_secret(&b) {
                return Ok(e);
            }
        }
    }

    /// Из 32 байт скаляра (тест-векторы, хранение между шагами).
    pub fn from_secret(b: &[u8; 32]) -> Result<Ephemeral, CryptoError> {
        let secret = SecretKey::from_slice(b).map_err(|_| CryptoError::BadRecord)?;
        let mut public = [0u8; PUBLIC_LEN];
        public.copy_from_slice(secret.public_key().to_sec1_point(false).as_bytes());
        Ok(Ephemeral { secret, public })
    }

    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes().into())
    }

    pub fn public(&self) -> &[u8; PUBLIC_LEN] {
        &self.public
    }

    /// Общий секрет ECDH (координата x) с чужим открытым ключом.
    fn shared(&self, peer: &[u8]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
        let peer = parse_public(peer)?;
        let s = self.secret.diffie_hellman(&peer);
        let mut out = Zeroizing::new([0u8; 32]);
        out.copy_from_slice(s.raw_secret_bytes());
        Ok(out)
    }
}

/// Открытый ключ обмена: только несжатая точка на кривой (не бесконечность).
fn parse_public(b: &[u8]) -> Result<PublicKey, CryptoError> {
    if b.len() != PUBLIC_LEN || b[0] != 0x04 {
        return Err(CryptoError::BadRecord);
    }
    PublicKey::from_sec1_bytes(b).map_err(|_| CryptoError::BadRecord)
}

/// Ключ годится как открытый ключ обмена.
pub fn valid_public(b: &[u8]) -> bool {
    parse_public(b).is_ok()
}

/// Обязательство источника: `sha256(A)`.
pub fn commit(public: &[u8]) -> [u8; 32] {
    Sha256::digest(public).into()
}

/// `A` — тот ключ, которым источник обязался.
pub fn check_commit(public: &[u8], commitment: &[u8]) -> bool {
    bool::from(commit(public).as_slice().ct_eq(commitment))
}

/// Длина `h` — префикса обязательства в QR.
pub const COMMIT_CHECK_LEN: usize = 16;

/// `h` для QR: первые 16 байт обязательства.
pub fn commit_check(commitment: &[u8]) -> [u8; COMMIT_CHECK_LEN] {
    let mut out = [0u8; COMMIT_CHECK_LEN];
    let n = commitment.len().min(COMMIT_CHECK_LEN);
    out[..n].copy_from_slice(&commitment[..n]);
    out
}

/// Обязательство от сервера совпадает с `h` из QR (за постоянное время).
pub fn check_commit_check(check: &[u8], commitment: &[u8]) -> bool {
    commitment.len() == 32
        && check.len() == COMMIT_CHECK_LEN
        && bool::from(commitment[..COMMIT_CHECK_LEN].ct_eq(check))
}

/// MAC открытого ключа нового устройства секретом из QR.
pub fn link_mac(link_secret: &[u8], public: &[u8]) -> [u8; 32] {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(link_secret).unwrap_or_else(|_| unreachable!());
    m.update(public);
    m.finalize().into_bytes().into()
}

pub fn check_mac(link_secret: &[u8], public: &[u8], mac: &[u8]) -> bool {
    bool::from(link_mac(link_secret, public).as_slice().ct_eq(mac))
}

/// Шесть цифр для сверки на обоих экранах (`a` — ключ источника, `b` — нового).
pub fn sas(labels: &Labels, a: &[u8], b: &[u8]) -> u32 {
    let h = Sha256::new()
        .chain_update(&labels.sas)
        .chain_update(a)
        .chain_update(b)
        .finalize();
    u32::from_be_bytes([h[0], h[1], h[2], h[3]]) % 1_000_000
}

/// Цифры строкой с ведущими нулями: `"042917"`.
pub fn sas_digits(labels: &Labels, a: &[u8], b: &[u8]) -> String {
    format!("{:06}", sas(labels, a, b))
}

fn wrap_key(labels: &Labels, shared: &[u8; 32], a: &[u8], b: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut salt = Vec::with_capacity(a.len() + b.len());
    salt.extend_from_slice(a);
    salt.extend_from_slice(b);
    let mut k = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), shared)
        .expand(&labels.key, k.as_mut())
        .unwrap_or_else(|_| unreachable!());
    k
}

/// Источник: зашифровать ключ для нового устройства (`joiner` — его `B`) с заданным
/// nonce. Nonce обязан быть одноразовым; без тест-векторов — [`wrap`].
pub fn wrap_with_nonce(
    labels: &Labels,
    source: &Ephemeral,
    joiner: &[u8],
    payload: &[u8],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, CryptoError> {
    let shared = source.shared(joiner)?;
    let k = wrap_key(labels, &shared, source.public(), joiner);
    let ct = Aes256Gcm::new_from_slice(k.as_ref())
        .unwrap_or_else(|_| unreachable!())
        .encrypt(nonce.into(), payload)
        .map_err(|_| CryptoError::Decrypt)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn wrap(
    labels: &Labels,
    source: &Ephemeral,
    joiner: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    wrap_with_nonce(labels, source, joiner, payload, &random_bytes()?)
}

/// Новое устройство: открыть обёртку ключом источника `A` (его обязательство уже
/// сверено). Подмена или порча — [`CryptoError::Decrypt`].
pub fn unwrap(
    labels: &Labels,
    joiner: &Ephemeral,
    source: &[u8],
    wrapped: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if wrapped.len() < NONCE_LEN + 16 {
        return Err(CryptoError::Decrypt);
    }
    let shared = joiner.shared(source)?;
    let k = wrap_key(labels, &shared, source, joiner.public());
    let (nonce, ct) = wrapped.split_at(NONCE_LEN);
    let nonce: &[u8; NONCE_LEN] = nonce.try_into().unwrap_or_else(|_| unreachable!());
    Aes256Gcm::new_from_slice(k.as_ref())
        .unwrap_or_else(|_| unreachable!())
        .decrypt(nonce.into(), ct)
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn base64url(b: &[u8]) -> String {
        crate::engine::base64url(b)
    }

    fn arr<const N: usize>(v: &serde_json::Value) -> [u8; N] {
        hex(v.as_str().unwrap()).try_into().unwrap()
    }

    /// Тест-векторы в `spec/vectors/handoff.json` — общие для ядра и плагинов
    /// (плагины сверяют их через WebCrypto).
    #[test]
    fn vectors_match() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../../../spec/vectors/handoff.json")).unwrap();
        let cases = v["cases"].as_array().unwrap();
        assert!(cases.len() >= 2);
        for c in cases {
            let labels = Labels::new(c["prefix"].as_str().unwrap());
            let a = Ephemeral::from_secret(&arr(&c["a_secret"])).unwrap();
            let b = Ephemeral::from_secret(&arr(&c["b_secret"])).unwrap();
            assert_eq!(to_hex(a.public()), c["a_public"].as_str().unwrap());
            assert_eq!(to_hex(b.public()), c["b_public"].as_str().unwrap());
            assert_eq!(to_hex(&commit(a.public())), c["commit"].as_str().unwrap());
            let h = commit_check(&commit(a.public()));
            assert_eq!(base64url(&h), c["h"].as_str().unwrap());
            assert!(check_commit_check(&h, &commit(a.public())));
            assert!(!check_commit_check(&h, &commit(b.public())));
            let m = hex(c["link_secret"].as_str().unwrap());
            assert_eq!(
                to_hex(&link_mac(&m, b.public())),
                c["mac"].as_str().unwrap()
            );
            assert_eq!(
                sas_digits(&labels, a.public(), b.public()),
                c["sas"].as_str().unwrap()
            );
            let payload = hex(c["payload"].as_str().unwrap());
            let w = wrap_with_nonce(&labels, &a, b.public(), &payload, &arr(&c["nonce"])).unwrap();
            assert_eq!(to_hex(&w), c["wrapped"].as_str().unwrap());
            assert_eq!(*unwrap(&labels, &b, a.public(), &w).unwrap(), payload);
        }
    }

    #[test]
    fn round_trip_and_tamper() {
        let l = Labels::selfsync();
        let a = Ephemeral::generate().unwrap();
        let b = Ephemeral::generate().unwrap();
        assert!(valid_public(a.public()));
        assert!(check_commit(a.public(), &commit(a.public())));
        assert!(!check_commit(b.public(), &commit(a.public())));
        let m = [9u8; LINK_SECRET_LEN];
        assert!(check_mac(&m, b.public(), &link_mac(&m, b.public())));
        assert!(!check_mac(
            &[8u8; 16],
            b.public(),
            &link_mac(&m, b.public())
        ));
        assert_eq!(
            sas(&l, a.public(), b.public()),
            sas(&l, a.public(), b.public())
        );
        assert!(sas(&l, a.public(), b.public()) < 1_000_000);

        let key = [7u8; 32];
        let w = wrap(&l, &a, b.public(), &key).unwrap();
        assert_eq!(w.len(), NONCE_LEN + 32 + 16);
        assert_eq!(*unwrap(&l, &b, a.public(), &w).unwrap(), key);
        // Чужая метка, чужой ключ источника, испорченная обёртка — не открываются.
        let cloud = Labels::new(CLOUD_PREFIX);
        assert_eq!(
            unwrap(&cloud, &b, a.public(), &w),
            Err(CryptoError::Decrypt)
        );
        let mallory = Ephemeral::generate().unwrap();
        assert_eq!(
            unwrap(&l, &b, mallory.public(), &w),
            Err(CryptoError::Decrypt)
        );
        let mut bad = w.clone();
        bad[20] ^= 1;
        assert_eq!(unwrap(&l, &b, a.public(), &bad), Err(CryptoError::Decrypt));
        assert_eq!(
            unwrap(&l, &b, a.public(), &w[..20]),
            Err(CryptoError::Decrypt)
        );
    }

    #[test]
    fn public_keys_validated() {
        let a = Ephemeral::generate().unwrap();
        let mut p = *a.public();
        assert!(valid_public(&p));
        p[0] = 0x02; // сжатая форма не принимается
        assert!(!valid_public(&p));
        assert!(!valid_public(&a.public()[..33]));
        let mut off = *a.public();
        off[64] ^= 1; // точка не на кривой
        assert!(!valid_public(&off));
        assert!(!valid_public(&[0x04; PUBLIC_LEN]));
        let b = Ephemeral::generate().unwrap();
        assert!(wrap(&Labels::selfsync(), &b, &off, b"k").is_err());
    }

    #[test]
    fn secret_round_trips() {
        let a = Ephemeral::generate().unwrap();
        let again = Ephemeral::from_secret(&a.secret_bytes()).unwrap();
        assert_eq!(again.public(), a.public());
        assert!(Ephemeral::from_secret(&[0u8; 32]).is_err());
        assert!(Ephemeral::from_secret(&[0xff; 32]).is_err());
    }
}
