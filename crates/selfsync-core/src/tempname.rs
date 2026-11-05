//! Имена временных файлов рядом с файлом назначения: атомарная запись
//! (`X.selfsync-tmp`, `X.commit.selfsync-tmp`) и двухшаговое переименование
//! (`X.case.selfsync-tmp`, `X.nfc-2.selfsync-tmp`).
//!
//! Имя файла в vault'е может занимать все 255 байт, и суффикс к нему уже не
//! помещается (ENAMETOOLONG). Тогда вместо `X` берётся укороченная база
//! `{начало имени}~{хэш пути}`: длина ограничена, а по хэшу ядро находит в индексе,
//! чьим был брошенный временный файл. Правило одно для ядра, CLI и плагина
//! (`plugin/src/io/backend.ts`, `tempBase`), поэтому хэш — простой FNV-1a.

use crate::path::MAX_SEGMENT_BYTES;

/// Суффикс временных файлов (исключается всегда).
pub const TEMP_SUFFIX: &str = ".selfsync-tmp";

/// Сколько байт имени оставить после базы: самый длинный хвост — `.case-99` или
/// `.commit` и `TEMP_SUFFIX` (21 байт), с запасом.
pub const RESERVE: usize = 24;

const HASH_HEX: usize = 16;

/// Предел байт для начала имени в укороченной базе (`~` и хэш — ещё 17 байт).
const PREFIX_MAX: usize = MAX_SEGMENT_BYTES - RESERVE - 1 - HASH_HEX;

/// FNV-1a, 64 бита, — 16 hex-цифр. Не криптография: только чтобы отличать пути.
pub fn path_hash(path: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Начало строки не длиннее `max` байт, по границе символа.
fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn split_name(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// База временного имени для `path` (к ней добавляются тег и `TEMP_SUFFIX`): сам
/// путь, если имя с хвостом укладывается в 255 байт, иначе
/// `{каталог}/{начало имени}~{хэш пути}`.
pub fn temp_base(path: &str) -> String {
    let (dir, name) = split_name(path);
    if name.len() + RESERVE <= MAX_SEGMENT_BYTES {
        return path.to_owned();
    }
    format!("{dir}{}~{}", cut(name, PREFIX_MAX), path_hash(path))
}

/// Хэш пути назначения, если `base` — укороченная база из `temp_base`.
pub fn shortened_hash(base: &str) -> Option<&str> {
    let (_, name) = split_name(base);
    let tail = name.len().checked_sub(HASH_HEX + 1)?;
    // Начало имени обрезано по границе символа: на 0–3 байта короче предела.
    if !(PREFIX_MAX - 3..=PREFIX_MAX).contains(&tail) {
        return None;
    }
    let hash = name.get(tail..)?.strip_prefix('~')?;
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    hash.bytes().all(hex).then_some(hash)
}

/// Имя, под которое атомарная запись отодвигает заменяемый файл `path`
/// (`IoResult::Displaced`): `{база}.aside.selfsync-tmp`, если занято — `.aside-N`.
pub fn aside(path: &str, n: u32) -> String {
    let base = temp_base(path);
    if n <= 1 {
        format!("{base}.aside{TEMP_SUFFIX}")
    } else {
        format!("{base}.aside-{n}{TEMP_SUFFIX}")
    }
}

/// Каталог пути (`a/b/c.md` → `a/b/`, `c.md` → ``).
pub fn dir_of(path: &str) -> &str {
    split_name(path).0
}

/// Имя не длиннее `max` байт: как есть, если помещается, иначе
/// `{начало}~{хэш имени}` с прежним расширением (если оно короткое).
pub fn fit_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let ext = name
        .rfind('.')
        .map(|i| &name[i..])
        .filter(|e| e.len() <= 16 && !e.contains(' '))
        .unwrap_or("");
    let budget = max.saturating_sub(1 + HASH_HEX + ext.len());
    format!("{}~{}{ext}", cut(name, budget), path_hash(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_vectors() {
        // Те же значения проверяет plugin/test/tempname.test.ts.
        assert_eq!(path_hash(""), "cbf29ce484222325");
        assert_eq!(path_hash("a.md"), "6ab57f8279f280cf");
        assert_eq!(path_hash("dir/заметка.md"), "b521fc211dc46709");
        let long = format!("dir/{}.md", "я".repeat(126));
        assert_eq!(
            temp_base(&long),
            format!("dir/{}~9e5f9fd5ffd34c3c", "я".repeat(107))
        );
    }

    #[test]
    fn short_names_unchanged() {
        assert_eq!(temp_base("a.md"), "a.md");
        assert_eq!(temp_base("dir/a.md"), "dir/a.md");
        let edge = "x".repeat(MAX_SEGMENT_BYTES - RESERVE);
        assert_eq!(temp_base(&edge), edge);
        assert_eq!(shortened_hash("a.md"), None);
    }

    #[test]
    fn long_names_bounded_and_recognised() {
        for name in [
            format!("{}.md", "a".repeat(252)),
            format!("{}.md", "я".repeat(126)),
            format!("{}.md", "😀".repeat(63)),
        ] {
            for path in [name.clone(), format!("dir/sub/{name}")] {
                let base = temp_base(&path);
                assert!(base.starts_with(dir_of(&path)));
                let seg = &base[dir_of(&base).len()..];
                let longest = format!("{seg}.case-99{TEMP_SUFFIX}");
                assert!(longest.len() <= MAX_SEGMENT_BYTES, "{}", longest.len());
                assert_eq!(shortened_hash(&base), Some(path_hash(&path).as_str()));
            }
        }
    }

    #[test]
    fn fit_name_keeps_extension() {
        assert_eq!(fit_name("a.md", 255), "a.md");
        let long = format!("{} - {}.md", "d".repeat(200), "я".repeat(100));
        let f = fit_name(&long, 251);
        assert!(f.len() <= 251);
        assert!(f.ends_with(".md"));
        assert!(f.is_char_boundary(f.len() - 3));
    }
}
