//! Пути в vault'е: валидация (общая для клиента и сервера), нормализация,
//! каноническое кодирование для БД и имена конфликтных копий.
//!
//! Сервер **отклоняет** невалидные открытые пути, а не нормализует их: клиентов разных
//! версий будет много, а разъехавшиеся не-ASCII имена потом уже не свести. Клиент
//! нормализует в NFC до отправки ([`VaultPath::normalize`]).

use std::fmt;

use selfsync_proto::v1 as pb;
use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, is_nfc};

/// Максимальная длина открытого сегмента, байт.
pub const MAX_SEGMENT_BYTES: usize = 255;
/// Максимальная длина открытого пути (сегменты через `/`), байт.
pub const MAX_PATH_BYTES: usize = 1024;
/// Максимальная длина зашифрованного сегмента (SIV-тег + паддинг), байт.
pub const MAX_ENCRYPTED_SEGMENT_BYTES: usize = 512;
/// Максимальное число сегментов в пути.
pub const MAX_SEGMENTS: usize = 64;

/// Байт режима в начале канонического кодирования.
const MODE_PLAIN: u8 = 0;
const MODE_ENCRYPTED: u8 = 1;

/// Причина отказа в пути. Коды стабильны: они уходят клиенту в `Rejected.code`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("пустой путь")]
    Empty,
    #[error("слишком много сегментов")]
    TooManySegments,
    #[error("пустой сегмент")]
    EmptySegment,
    #[error("сегмент `.` или `..`")]
    DotSegment,
    #[error("сегмент не в UTF-8")]
    NotUtf8,
    #[error("сегмент не в NFC")]
    NotNfc,
    #[error("управляющий символ в сегменте")]
    ControlChar,
    #[error("разделитель внутри сегмента")]
    Separator,
    #[error("пробел или точка в конце сегмента")]
    TrailingSpaceOrDot,
    #[error("сегмент длиннее {MAX_SEGMENT_BYTES} байт")]
    SegmentTooLong,
    #[error("путь длиннее {MAX_PATH_BYTES} байт")]
    PathTooLong,
    #[error("повреждённое каноническое кодирование")]
    BadEncoding,
}

impl PathError {
    /// Стабильный машинный код для протокола.
    pub fn code(&self) -> &'static str {
        match self {
            PathError::Empty => "path_empty",
            PathError::TooManySegments => "path_too_deep",
            PathError::EmptySegment => "path_empty_segment",
            PathError::DotSegment => "path_dot_segment",
            PathError::NotUtf8 => "path_not_utf8",
            PathError::NotNfc => "path_not_nfc",
            PathError::ControlChar => "path_control_char",
            PathError::Separator => "path_separator",
            PathError::TrailingSpaceOrDot => "path_trailing_space_or_dot",
            PathError::SegmentTooLong => "path_segment_too_long",
            PathError::PathTooLong => "path_too_long",
            PathError::BadEncoding => "path_bad_encoding",
        }
    }
}

/// Проверяет один открытый сегмент по правилам раздела 6.6.
pub fn validate_segment(seg: &str) -> Result<(), PathError> {
    if seg.is_empty() {
        return Err(PathError::EmptySegment);
    }
    if seg == "." || seg == ".." {
        return Err(PathError::DotSegment);
    }
    if seg.len() > MAX_SEGMENT_BYTES {
        return Err(PathError::SegmentTooLong);
    }
    for ch in seg.chars() {
        if ch == '/' || ch == '\\' {
            return Err(PathError::Separator);
        }
        // C0, DEL и C1: в именах файлов их не бывает намеренно, а Windows их запрещает.
        if ch.is_control() {
            return Err(PathError::ControlChar);
        }
    }
    if seg.ends_with(' ') || seg.ends_with('.') {
        return Err(PathError::TrailingSpaceOrDot);
    }
    if !is_nfc(seg) {
        return Err(PathError::NotNfc);
    }
    Ok(())
}

/// Проверяет зашифрованный сегмент: сервер видит только длину.
pub fn validate_encrypted_segment(seg: &[u8]) -> Result<(), PathError> {
    if seg.is_empty() {
        return Err(PathError::EmptySegment);
    }
    if seg.len() > MAX_ENCRYPTED_SEGMENT_BYTES {
        return Err(PathError::SegmentTooLong);
    }
    Ok(())
}

/// Валидный открытый путь: сегменты в NFC, соединённые `/`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VaultPath(String);

impl VaultPath {
    /// Строгий разбор: путь уже должен быть в NFC (так проверяет сервер).
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.len() > MAX_PATH_BYTES {
            return Err(PathError::PathTooLong);
        }
        let mut n = 0;
        for seg in s.split('/') {
            validate_segment(seg)?;
            n += 1;
        }
        if n > MAX_SEGMENTS {
            return Err(PathError::TooManySegments);
        }
        Ok(VaultPath(s.to_owned()))
    }

    /// Клиентская нормализация: NFC, а затем строгая проверка. Ведущие и
    /// замыкающие `/` отбрасываются; всё остальное, что не проходит
    /// [`VaultPath::parse`], — ошибка, которую клиент показывает пользователю.
    pub fn normalize(s: &str) -> Result<Self, PathError> {
        let trimmed = s.trim_matches('/');
        let nfc: String = trimmed.nfc().collect();
        Self::parse(&nfc)
    }

    /// Разбор сегментов из протокола (открытый путь).
    pub fn from_segments<S: AsRef<[u8]>>(segs: &[S]) -> Result<Self, PathError> {
        if segs.is_empty() {
            return Err(PathError::Empty);
        }
        if segs.len() > MAX_SEGMENTS {
            return Err(PathError::TooManySegments);
        }
        let mut out = String::new();
        for (i, seg) in segs.iter().enumerate() {
            let s = std::str::from_utf8(seg.as_ref()).map_err(|_| PathError::NotUtf8)?;
            validate_segment(s)?;
            if i > 0 {
                out.push('/');
            }
            out.push_str(s);
        }
        if out.len() > MAX_PATH_BYTES {
            return Err(PathError::PathTooLong);
        }
        Ok(VaultPath(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Последний сегмент.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Родительский каталог (None для файла в корне).
    pub fn parent(&self) -> Option<VaultPath> {
        self.0.rfind('/').map(|i| VaultPath(self.0[..i].to_owned()))
    }

    /// Все предки от корня: `a/b/c` → `a`, `a/b`.
    pub fn ancestors(&self) -> Vec<VaultPath> {
        let mut out = Vec::new();
        for (i, ch) in self.0.char_indices() {
            if ch == '/' {
                out.push(VaultPath(self.0[..i].to_owned()));
            }
        }
        out
    }

    /// Лежит ли путь внутри каталога `dir` (не равен ему).
    pub fn is_inside(&self, dir: &VaultPath) -> bool {
        self.0.len() > dir.0.len()
            && self.0.starts_with(&dir.0)
            && self.0.as_bytes()[dir.0.len()] == b'/'
    }

    /// Заменяет префикс-каталог `from` на `to` (для переименования папок).
    pub fn rebase(&self, from: &VaultPath, to: &VaultPath) -> Option<VaultPath> {
        if self == from {
            return Some(to.clone());
        }
        if self.is_inside(from) {
            let rest = &self.0[from.0.len()..];
            return VaultPath::parse(&format!("{}{}", to.0, rest)).ok();
        }
        None
    }

    /// Ключ для сравнения без учёта регистра (обнаружение коллизий на
    /// регистронезависимых ФС).
    pub fn casefold(&self) -> String {
        self.0.to_lowercase()
    }

    /// Переименование, меняющее только регистр.
    pub fn is_case_only_change(&self, other: &VaultPath) -> bool {
        self != other && self.casefold() == other.casefold()
    }

    /// Расширение последнего сегмента в нижнем регистре (без точки).
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        let (stem, ext) = split_ext(name);
        if stem.is_empty() || ext.is_empty() {
            None
        } else {
            Some(ext.to_lowercase())
        }
    }

    /// Похоже ли на заметку (для приоритета в очереди).
    pub fn is_note(&self) -> bool {
        matches!(
            self.extension().as_deref(),
            Some("md" | "txt" | "canvas" | "base")
        )
    }

    /// Путь копии рядом с файлом: `Заметка (метка).md`. Метка очищается от
    /// запрещённых символов, имя укорачивается до лимита сегмента.
    pub fn with_suffix(&self, label: &str) -> VaultPath {
        let label = sanitize_label(label);
        let name = self.file_name();
        let (stem, ext) = split_ext(name);
        let tail = if ext.is_empty() {
            format!(" ({label})")
        } else {
            format!(" ({label}).{ext}")
        };
        let budget = MAX_SEGMENT_BYTES.saturating_sub(tail.len());
        let mut stem_cut = String::new();
        for ch in stem.chars() {
            if stem_cut.len() + ch.len_utf8() > budget {
                break;
            }
            stem_cut.push(ch);
        }
        let mut new_name = format!("{stem_cut}{tail}");
        if validate_segment(&new_name).is_err() {
            // Метка уже очищена, остаётся только экзотика вроде NFD в исходном
            // имени, которого быть не может: путь валиден по построению.
            new_name = format!("copy{tail}");
        }
        let joined = match self.parent() {
            Some(p) => format!("{}/{}", p.0, new_name),
            None => new_name,
        };
        VaultPath::parse(&joined).unwrap_or_else(|_| self.clone())
    }

    /// Путь в протокол (открытый).
    pub fn to_proto(&self) -> pb::Path {
        pb::Path {
            segments: self.segments().map(|s| s.as_bytes().to_vec()).collect(),
            encrypted: false,
        }
    }
}

impl fmt::Debug for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `name.ext` → (`name`, `ext`); у `.hidden` и `noext` расширения нет.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(i) => (&name[..i], &name[i + 1..]),
    }
}

fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .nfc()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() || ":*?\"<>|".contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_end_matches([' ', '.']).trim_start();
    if trimmed.is_empty() {
        "copy".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Проверка пути из протокола с учётом режима. Возвращает открытый путь,
/// если путь не зашифрован.
pub fn validate_proto_path(p: &pb::Path) -> Result<Option<VaultPath>, PathError> {
    if p.encrypted {
        if p.segments.is_empty() {
            return Err(PathError::Empty);
        }
        if p.segments.len() > MAX_SEGMENTS {
            return Err(PathError::TooManySegments);
        }
        for s in &p.segments {
            validate_encrypted_segment(s)?;
        }
        Ok(None)
    } else {
        VaultPath::from_segments(&p.segments).map(Some)
    }
}

/// Каноническое кодирование пути для БД: байт режима, затем на каждый сегмент
/// u16 BE длины и байты. Префикс каталога — байтовый префикс кодирования пути
/// внутри него, одинаково для открытых и зашифрованных путей.
pub fn canonical_encode(p: &pb::Path) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + p.segments.iter().map(|s| s.len() + 2).sum::<usize>());
    out.push(if p.encrypted {
        MODE_ENCRYPTED
    } else {
        MODE_PLAIN
    });
    for s in &p.segments {
        // Длина ограничена валидацией (≤ 512), u16 хватает.
        let len = u16::try_from(s.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&s[..usize::from(len)]);
    }
    out
}

/// Обратное к [`canonical_encode`].
pub fn canonical_decode(bytes: &[u8]) -> Result<pb::Path, PathError> {
    let (&mode, mut rest) = bytes.split_first().ok_or(PathError::BadEncoding)?;
    let encrypted = match mode {
        MODE_PLAIN => false,
        MODE_ENCRYPTED => true,
        _ => return Err(PathError::BadEncoding),
    };
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if rest.len() < 2 {
            return Err(PathError::BadEncoding);
        }
        let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
        rest = &rest[2..];
        if rest.len() < len {
            return Err(PathError::BadEncoding);
        }
        segments.push(rest[..len].to_vec());
        rest = &rest[len..];
    }
    Ok(pb::Path {
        segments,
        encrypted,
    })
}

/// Канонический префикс «всё внутри каталога».
pub fn canonical_dir_prefix(p: &pb::Path) -> Vec<u8> {
    canonical_encode(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_paths() {
        for p in [
            "note.md",
            "Папка/Заметка.md",
            "a/b/c/d.png",
            ".obsidian/app.json",
            "日本語/ノート.md",
            "with space/file name.md",
        ] {
            assert!(VaultPath::parse(p).is_ok(), "{p}");
        }
    }

    #[test]
    fn rejects_bad_paths() {
        let cases: &[(&str, PathError)] = &[
            ("", PathError::Empty),
            ("/abs.md", PathError::EmptySegment),
            ("a//b", PathError::EmptySegment),
            ("a/", PathError::EmptySegment),
            ("../etc/passwd", PathError::DotSegment),
            ("a/./b", PathError::DotSegment),
            ("a\\b", PathError::Separator),
            ("bad\u{0}nul", PathError::ControlChar),
            ("tab\there", PathError::ControlChar),
            ("del\u{7f}", PathError::ControlChar),
            ("trailing ", PathError::TrailingSpaceOrDot),
            ("dir./x", PathError::TrailingSpaceOrDot),
            ("e\u{301}.md", PathError::NotNfc),
        ];
        for (p, e) in cases {
            assert_eq!(VaultPath::parse(p).unwrap_err(), *e, "{p:?}");
        }
    }

    #[test]
    fn lengths() {
        let seg = "a".repeat(255);
        assert!(VaultPath::parse(&seg).is_ok());
        assert_eq!(
            VaultPath::parse(&"a".repeat(256)).unwrap_err(),
            PathError::SegmentTooLong
        );
        // 5 сегментов по 250 байт > 1024
        let long = vec!["b".repeat(250); 5].join("/");
        assert_eq!(VaultPath::parse(&long).unwrap_err(), PathError::PathTooLong);
        // многобайтовые символы считаются в байтах
        let cyr = "я".repeat(128); // 256 байт
        assert_eq!(
            VaultPath::parse(&cyr).unwrap_err(),
            PathError::SegmentTooLong
        );
    }

    #[test]
    fn normalize_nfd_to_nfc() {
        let nfd = "Cafe\u{301}/Re\u{301}sume\u{301}.md";
        assert!(VaultPath::parse(nfd).is_err());
        let p = VaultPath::normalize(nfd).unwrap();
        assert_eq!(p.as_str(), "Café/Résumé.md");
        assert_eq!(VaultPath::normalize("/a/b/").unwrap().as_str(), "a/b");
    }

    #[test]
    fn case_only() {
        let a = VaultPath::parse("dir/note.md").unwrap();
        let b = VaultPath::parse("dir/Note.md").unwrap();
        assert!(a.is_case_only_change(&b));
        assert!(!a.is_case_only_change(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn suffix_names() {
        let p = VaultPath::parse("dir/Заметка.md").unwrap();
        assert_eq!(
            p.with_suffix("conflict 2026-10-09 14:30 iPhone").as_str(),
            "dir/Заметка (conflict 2026-10-09 14-30 iPhone).md"
        );
        let hidden = VaultPath::parse(".gitignore").unwrap();
        assert_eq!(hidden.with_suffix("x").as_str(), ".gitignore (x)");
        let weird = VaultPath::parse("a.md").unwrap();
        assert_eq!(
            weird.with_suffix("dev/../ice. ").as_str(),
            "a (dev-..-ice).md"
        );
        let long = VaultPath::parse(&format!("{}.md", "я".repeat(120))).unwrap();
        let c = long.with_suffix("conflict");
        assert!(c.file_name().len() <= MAX_SEGMENT_BYTES);
        assert!(c.as_str().ends_with(" (conflict).md"));
    }

    #[test]
    fn hierarchy_helpers() {
        let p = VaultPath::parse("a/b/c.md").unwrap();
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        assert_eq!(p.file_name(), "c.md");
        let anc: Vec<_> = p.ancestors().iter().map(|x| x.to_string()).collect();
        assert_eq!(anc, ["a", "a/b"]);
        let ab = VaultPath::parse("a/b").unwrap();
        assert!(p.is_inside(&ab));
        assert!(!VaultPath::parse("a/bc").unwrap().is_inside(&ab));
        let moved = p.rebase(&ab, &VaultPath::parse("x").unwrap()).unwrap();
        assert_eq!(moved.as_str(), "x/c.md");
        assert_eq!(p.extension().as_deref(), Some("md"));
        assert!(p.is_note());
    }

    #[test]
    fn canonical_roundtrip_and_prefix() {
        let dir = VaultPath::parse("a/b").unwrap().to_proto();
        let file = VaultPath::parse("a/b/c.md").unwrap().to_proto();
        let sibling = VaultPath::parse("a/bc").unwrap().to_proto();
        let e_dir = canonical_encode(&dir);
        assert!(canonical_encode(&file).starts_with(&e_dir));
        assert!(!canonical_encode(&sibling).starts_with(&e_dir));
        assert_eq!(canonical_decode(&canonical_encode(&file)).unwrap(), file);

        let enc = pb::Path {
            segments: vec![vec![0u8, 1, 2], vec![255; 32]],
            encrypted: true,
        };
        let e = canonical_encode(&enc);
        assert_eq!(e[0], MODE_ENCRYPTED);
        assert_eq!(canonical_decode(&e).unwrap(), enc);
        assert!(canonical_decode(&[7]).is_err());
        assert!(canonical_decode(&[0, 0, 5, b'a']).is_err());
    }

    #[test]
    fn proto_validation() {
        let ok = pb::Path {
            segments: vec![b"a".to_vec(), "é".as_bytes().to_vec()],
            encrypted: false,
        };
        assert_eq!(validate_proto_path(&ok).unwrap().unwrap().as_str(), "a/é");
        let bad_utf8 = pb::Path {
            segments: vec![vec![0xff]],
            encrypted: false,
        };
        assert_eq!(
            validate_proto_path(&bad_utf8).unwrap_err(),
            PathError::NotUtf8
        );
        let with_slash = pb::Path {
            segments: vec![b"a/b".to_vec()],
            encrypted: false,
        };
        assert_eq!(
            validate_proto_path(&with_slash).unwrap_err(),
            PathError::Separator
        );
        let enc = pb::Path {
            segments: vec![vec![0u8; 48]],
            encrypted: true,
        };
        assert!(validate_proto_path(&enc).unwrap().is_none());
        let enc_bad = pb::Path {
            segments: vec![vec![]],
            encrypted: true,
        };
        assert!(validate_proto_path(&enc_bad).is_err());
    }
}
