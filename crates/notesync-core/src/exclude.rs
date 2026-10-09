//! Исключения из синка: не синкаются ни в одну сторону.
//!
//! Синтаксис шаблона (по одному на строку настроек):
//! - `dir/` — каталог и всё внутри;
//! - `path/file.json` — ровно этот путь (а если это каталог — и всё внутри);
//! - `*` внутри сегмента — любые символы, кроме `/`; `**` — любое число сегментов;
//! - шаблон без `/` и с `*` применяется к имени в любом каталоге (`*.tmp`);
//! - пустые строки и строки с `#` игнорируются.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Pattern {
    /// Точный путь или каталог.
    Exact(String),
    /// Глоб по сегментам.
    Glob(Vec<String>),
    /// Глоб по имени в любом каталоге.
    Name(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excludes {
    patterns: Vec<Pattern>,
}

impl Excludes {
    pub fn new<S: AsRef<str>>(lines: &[S]) -> Self {
        let mut patterns = Vec::new();
        for line in lines {
            let l = line.as_ref().trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let l = l.trim_start_matches('/');
            let body = l.trim_end_matches('/');
            if body.is_empty() {
                continue;
            }
            if !body.contains('*') {
                patterns.push(Pattern::Exact(body.to_owned()));
            } else if !body.contains('/') {
                patterns.push(Pattern::Name(body.to_owned()));
            } else {
                patterns.push(Pattern::Glob(body.split('/').map(str::to_owned).collect()));
            }
        }
        Excludes { patterns }
    }

    /// Исключения по умолчанию относительно каталога настроек Obsidian.
    pub fn default_lines(config_dir: &str) -> Vec<String> {
        vec![
            format!("{config_dir}/workspace.json"),
            format!("{config_dir}/workspace-mobile.json"),
            ".trash/".to_owned(),
        ]
    }

    pub fn push(&mut self, line: &str) {
        let extra = Excludes::new(&[line]);
        self.patterns.extend(extra.patterns);
    }

    pub fn is_excluded(&self, path: &str) -> bool {
        self.patterns.iter().any(|p| match p {
            Pattern::Exact(e) => {
                path == e
                    || (path.len() > e.len()
                        && path.starts_with(e.as_str())
                        && path.as_bytes()[e.len()] == b'/')
            }
            Pattern::Name(n) => path.split('/').any(|seg| glob_segment(n, seg)),
            Pattern::Glob(g) => {
                let segs: Vec<&str> = path.split('/').collect();
                // Шаблон совпадает с путём или с одним из его предков (каталог).
                (1..=segs.len()).any(|n| glob_path(g, &segs[..n]))
            }
        })
    }
}

fn glob_path(pat: &[String], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=segs.len()).any(|k| glob_path(rest, &segs[k..]))
        }
        Some((first, rest)) => match segs.split_first() {
            Some((s, srest)) => glob_segment(first, s) && glob_path(rest, srest),
            None => false,
        },
    }
}

/// `*` — любые символы внутри сегмента.
fn glob_segment(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let e = Excludes::new(&Excludes::default_lines(".obsidian"));
        assert!(e.is_excluded(".obsidian/workspace.json"));
        assert!(e.is_excluded(".obsidian/workspace-mobile.json"));
        assert!(e.is_excluded(".trash/old.md"));
        assert!(e.is_excluded(".trash"));
        assert!(!e.is_excluded(".obsidian/app.json"));
        assert!(!e.is_excluded("notes/.trash.md"));
        assert!(!e.is_excluded("workspace.json"));
    }

    #[test]
    fn globs() {
        let e = Excludes::new(&["*.tmp", "build/**/cache", "# comment", "", "a/*/x"]);
        assert!(e.is_excluded("foo.tmp"));
        assert!(e.is_excluded("deep/dir/foo.tmp"));
        assert!(!e.is_excluded("foo.tmpl"));
        assert!(e.is_excluded("build/cache"));
        assert!(e.is_excluded("build/a/b/cache/file"));
        assert!(e.is_excluded("a/q/x"));
        assert!(e.is_excluded("a/q/x/inner"));
        assert!(!e.is_excluded("a/q/y"));
    }

    #[test]
    fn exact_dir_without_slash() {
        let e = Excludes::new(&["private"]);
        assert!(e.is_excluded("private"));
        assert!(e.is_excluded("private/a.md"));
        assert!(!e.is_excluded("private2/a.md"));
    }

    #[test]
    fn segment_glob() {
        assert!(glob_segment("a*b*c", "aXXbYYc"));
        assert!(!glob_segment("a*b*c", "aXXbYY"));
        assert!(glob_segment("*", ""));
        assert!(glob_segment("*.md", ".md"));
    }
}
