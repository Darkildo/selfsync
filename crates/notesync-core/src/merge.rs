//! Построчный 3-way merge (diff3) для текстов ≤ 1 МиБ.
//!
//! Правки в разных местах сливаются молча. Пересекающиеся (а также соседние, без
//! разделяющей их неизменной строки — так же ведут себя diff3 и git) дают
//! [`Merge::Conflict`]: тогда вызывающий сохраняет обе версии целиком.
//!
//! Переводы строк: тексты сравниваются по строкам без `\r`, поэтому смена `\n` ↔ `\r\n`
//! одной стороной не превращает весь файл в конфликт. В результате используется стиль
//! той стороны, которая его поменяла (иначе — стиль базы). Наличие завершающего
//! перевода строки сливается так же, как трёхсторонний скаляр.

use similar::{Algorithm, DiffOp, capture_diff_slices};

/// Лимит размера для слияния.
pub const MERGE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// Слито без конфликтов.
    Clean(String),
    /// Пересекающиеся правки.
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eol {
    Lf,
    CrLf,
}

struct Doc<'a> {
    lines: Vec<&'a str>,
    eol: Eol,
    trailing_newline: bool,
}

fn split(text: &str) -> Doc<'_> {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    let eol = if crlf > 0 && crlf * 2 >= lf {
        Eol::CrLf
    } else {
        Eol::Lf
    };
    let trailing_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines = if text.is_empty() {
        Vec::new()
    } else {
        body.split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect()
    };
    Doc {
        lines,
        eol,
        trailing_newline,
    }
}

/// Для каждой строки базы — индекс совпадающей строки в другой версии (по LCS).
fn matches(base: &[&str], other: &[&str]) -> Vec<Option<usize>> {
    let mut m = vec![None; base.len()];
    for op in capture_diff_slices(Algorithm::Myers, base, other) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                m[old_index + k] = Some(new_index + k);
            }
        }
    }
    m
}

fn pick3<T: PartialEq + Copy>(base: T, a: T, b: T) -> T {
    if a != base { a } else { b }
}

/// Сливает `ours` и `theirs` относительно общей базы.
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Merge {
    if ours == theirs {
        return Merge::Clean(ours.to_owned());
    }
    if ours == base {
        return Merge::Clean(theirs.to_owned());
    }
    if theirs == base {
        return Merge::Clean(ours.to_owned());
    }
    let o = split(base);
    let a = split(ours);
    let b = split(theirs);
    let ma = matches(&o.lines, &a.lines);
    let mb = matches(&o.lines, &b.lines);

    let (no, na, nb) = (o.lines.len(), a.lines.len(), b.lines.len());
    let mut out: Vec<&str> = Vec::with_capacity(na.max(nb));
    let (mut i, mut ja, mut jb) = (0usize, 0usize, 0usize);
    while i < no || ja < na || jb < nb {
        // Стабильная строка: совпадает во всех трёх версиях на текущих позициях.
        if i < no && ma[i] == Some(ja) && mb[i] == Some(jb) {
            out.push(o.lines[i]);
            i += 1;
            ja += 1;
            jb += 1;
            continue;
        }
        // Следующая строка базы, совпавшая в обеих версиях, закрывает нестабильный блок.
        let mut k = i;
        let (ea, eb) = loop {
            if k >= no {
                break (na, nb);
            }
            if let (Some(x), Some(y)) = (ma[k], mb[k])
                && x >= ja
                && y >= jb
            {
                break (x, y);
            }
            k += 1;
        };
        let oc = &o.lines[i..k];
        let ac = &a.lines[ja..ea];
        let bc = &b.lines[jb..eb];
        if ac == oc {
            out.extend_from_slice(bc);
        } else if bc == oc || ac == bc {
            out.extend_from_slice(ac);
        } else {
            return Merge::Conflict;
        }
        i = k;
        ja = ea;
        jb = eb;
    }

    let eol = pick3(o.eol, a.eol, b.eol);
    let trailing = pick3(o.trailing_newline, a.trailing_newline, b.trailing_newline);
    let sep = match eol {
        Eol::Lf => "\n",
        Eol::CrLf => "\r\n",
    };
    let mut s = out.join(sep);
    if trailing && !out.is_empty() {
        s.push_str(sep);
    }
    Merge::Clean(s)
}

/// Можно ли сливать эти байты построчно.
pub fn mergeable(data: &[u8]) -> bool {
    data.len() <= MERGE_LIMIT && crate::blob::is_text(data)
}

/// Слияние байтов: `None`, если хотя бы одна версия не текст или больше лимита.
pub fn merge_bytes(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Merge> {
    if !(mergeable(base) && mergeable(ours) && mergeable(theirs)) {
        return None;
    }
    // mergeable гарантирует UTF-8.
    let (b, o, t) = (
        std::str::from_utf8(base).ok()?,
        std::str::from_utf8(ours).ok()?,
        std::str::from_utf8(theirs).ok()?,
    );
    Some(merge3(b, o, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(m: Merge) -> String {
        match m {
            Merge::Clean(s) => s,
            Merge::Conflict => panic!("ожидался чистый merge"),
        }
    }

    #[test]
    fn non_overlapping_edits_merge() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\n";
        assert_eq!(clean(merge3(base, ours, theirs)), "A\nb\nc\nd\nE\n");
    }

    #[test]
    fn insertions_in_different_places() {
        let base = "one\ntwo\nthree\nfour\n";
        let ours = "zero\none\ntwo\nthree\nfour\n";
        let theirs = "one\ntwo\nthree\nfour\nfive\n";
        assert_eq!(
            clean(merge3(base, ours, theirs)),
            "zero\none\ntwo\nthree\nfour\nfive\n"
        );
    }

    #[test]
    fn overlapping_edits_conflict() {
        let base = "a\nb\nc\n";
        assert_eq!(merge3(base, "a\nX\nc\n", "a\nY\nc\n"), Merge::Conflict);
        // соседние строки без стабильной между ними — тоже конфликт
        assert_eq!(merge3(base, "A\nb\nc\n", "a\nB\nc\n"), Merge::Conflict);
        // обе стороны дописали разное в конец
        assert_eq!(
            merge3(base, "a\nb\nc\nx\n", "a\nb\nc\ny\n"),
            Merge::Conflict
        );
    }

    #[test]
    fn identical_changes_merge() {
        let base = "a\nb\nc\n";
        assert_eq!(clean(merge3(base, "a\nZ\nc\n", "a\nZ\nc\n")), "a\nZ\nc\n");
        let ours = "a\nZ\nc\nd\n";
        let theirs = "q\na\nZ\nc\n";
        assert_eq!(clean(merge3(base, ours, theirs)), "q\na\nZ\nc\nd\n");
    }

    #[test]
    fn deletion_vs_edit_elsewhere() {
        let base = "1\n2\n3\n4\n5\n";
        let ours = "1\n3\n4\n5\n";
        let theirs = "1\n2\n3\n4\nfive\n";
        assert_eq!(clean(merge3(base, ours, theirs)), "1\n3\n4\nfive\n");
    }

    #[test]
    fn empty_files() {
        assert_eq!(clean(merge3("", "", "x\n")), "x\n");
        assert_eq!(clean(merge3("", "a\n", "")), "a\n");
        assert_eq!(merge3("", "a\n", "b\n"), Merge::Conflict);
        assert_eq!(clean(merge3("a\n", "", "a\n")), "");
    }

    #[test]
    fn crlf_vs_lf() {
        let base = "a\nb\nc\nd\n";
        // одна сторона перевела файл в CRLF и ничего не меняла
        let ours = "a\r\nb\r\nc\r\nd\r\n";
        let theirs = "a\nb\nc\nD\n";
        assert_eq!(clean(merge3(base, ours, theirs)), "a\r\nb\r\nc\r\nD\r\n");
        // обе в CRLF, правки в разных местах
        let base = "a\r\nb\r\nc\r\nd\r\n";
        let ours = "A\r\nb\r\nc\r\nd\r\n";
        let theirs = "a\r\nb\r\nc\r\nD\r\n";
        assert_eq!(clean(merge3(base, ours, theirs)), "A\r\nb\r\nc\r\nD\r\n");
    }

    #[test]
    fn missing_trailing_newline() {
        let base = "a\nb\nc";
        let ours = "A\nb\nc";
        let theirs = "a\nb\nC";
        assert_eq!(clean(merge3(base, ours, theirs)), "A\nb\nC");
        // одна сторона добавила перевод строки в конце, другая правила начало
        let theirs = "a\nb\nc\n";
        let ours = "A\nb\nc";
        assert_eq!(clean(merge3(base, ours, theirs)), "A\nb\nc\n");
    }

    #[test]
    fn bytes_guard() {
        assert!(merge_bytes(b"a", b"b", &[0, 1]).is_none());
        let big = vec![b'a'; MERGE_LIMIT + 1];
        assert!(merge_bytes(b"a", &big, b"a").is_none());
        assert_eq!(
            merge_bytes(b"a\n", b"a\n", b"b\n"),
            Some(Merge::Clean("b\n".into()))
        );
    }

    #[test]
    fn edits_are_preserved_randomized() {
        // Свойство: если каждая сторона правит свои непересекающиеся строки
        // (с промежутком), результат содержит правки обеих.
        for n in 6..40usize {
            let base: Vec<String> = (0..n).map(|i| format!("line {i}")).collect();
            for gap in 2..4 {
                let ai = n / 4;
                let bi = ai + gap;
                if bi >= n {
                    continue;
                }
                let mut a = base.clone();
                a[ai] = format!("ours {ai}");
                let mut b = base.clone();
                b[bi] = format!("theirs {bi}");
                let m = clean(merge3(
                    &(base.join("\n") + "\n"),
                    &(a.join("\n") + "\n"),
                    &(b.join("\n") + "\n"),
                ));
                assert!(m.contains(&format!("ours {ai}")));
                assert!(m.contains(&format!("theirs {bi}")));
            }
        }
    }
}
