//! Исключения из синка: не синкаются ни в одну сторону.
//!
//! Синтаксис шаблона (по одному на строку настроек):
//! - `dir/` — каталог и всё внутри;
//! - `path/file.json` — ровно этот путь (а если это каталог — и всё внутри);
//! - `*` внутри сегмента — любые символы, кроме `/`; `**` — любое число сегментов;
//! - шаблон без `/` и с `*` применяется к имени в любом каталоге (`*.tmp`);
//! - `!шаблон` — повторно включить (как в gitignore): решает последнее совпавшее
//!   правило, так что `.obsidian/` + `!.obsidian/hotkeys.json` синхронизирует из
//!   каталога настроек только горячие клавиши;
//! - пустые строки и строки с `#` игнорируются.
//!
//! Жёсткие исключения ([`Excludes::push_hard`]: каталог плагина с токеном,
//! служебные каталоги) повторно включить нельзя.
//!
//! Каталог, внутри которого повторно включённый путь может оказаться, сам
//! исключённым не считается: иначе его содержимое пропало бы вместе с ним (папку
//! синк видит отдельной записью, а её «исчезновение» — как удаление всего внутри).

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

impl Pattern {
    fn parse(line: &str) -> Option<Pattern> {
        let l = line.trim_start_matches('/');
        let body = l.trim_end_matches('/');
        if body.is_empty() {
            return None;
        }
        Some(if !body.contains('*') {
            Pattern::Exact(body.to_owned())
        } else if !body.contains('/') {
            Pattern::Name(body.to_owned())
        } else {
            Pattern::Glob(body.split('/').map(str::to_owned).collect())
        })
    }

    /// Совпадает с путём или с одним из его предков.
    fn matches(&self, path: &str) -> bool {
        match self {
            Pattern::Exact(e) => {
                path == e
                    || (path.len() > e.len()
                        && path.starts_with(e.as_str())
                        && path.as_bytes()[e.len()] == b'/')
            }
            Pattern::Name(n) => path.split('/').any(|seg| glob_segment(n, seg)),
            Pattern::Glob(g) => {
                let segs: Vec<&str> = path.split('/').collect();
                (1..=segs.len()).any(|n| glob_path(g, &segs[..n]))
            }
        }
    }

    /// Указывает внутрь каталога `dir`: совпадёт с каким-то путём строго в нём.
    /// Шаблон по имени (`*.md`) никуда конкретно не указывает — для него `false`,
    /// иначе каждый исключённый файл считался бы «каталогом с включённым внутри».
    fn may_match_inside(&self, dir: &str) -> bool {
        match self {
            Pattern::Exact(e) => {
                e.len() > dir.len() && e.starts_with(dir) && e.as_bytes()[dir.len()] == b'/'
            }
            Pattern::Name(_) => false,
            Pattern::Glob(g) => {
                let segs: Vec<&str> = dir.split('/').collect();
                glob_extends(g, &segs)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Rule {
    /// `!шаблон`: повторно включить.
    include: bool,
    pattern: Pattern,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excludes {
    rules: Vec<Rule>,
    /// Жёсткие исключения: `!` их не отменяет.
    hard: Vec<Pattern>,
}

impl Excludes {
    pub fn new<S: AsRef<str>>(lines: &[S]) -> Self {
        let mut e = Excludes::default();
        for line in lines {
            e.push(line.as_ref());
        }
        e
    }

    /// Исключения по умолчанию относительно каталога настроек Obsidian.
    pub fn default_lines(config_dir: &str) -> Vec<String> {
        vec![
            format!("{config_dir}/workspace.json"),
            format!("{config_dir}/workspace-mobile.json"),
            ".trash/".to_owned(),
        ]
    }

    /// Строка настроек: шаблон, `!шаблон`, комментарий или пустая.
    pub fn push(&mut self, line: &str) {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            return;
        }
        let (include, body) = match l.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, l),
        };
        if let Some(pattern) = Pattern::parse(body) {
            self.rules.push(Rule { include, pattern });
        }
    }

    /// Жёсткое исключение (`!` в строке не действует).
    pub fn push_hard(&mut self, line: &str) {
        let l = line.trim().trim_start_matches('!');
        if let Some(p) = Pattern::parse(l) {
            self.hard.push(p);
        }
    }

    /// Жёстко исключён: никакое правило его не вернёт.
    pub fn is_hard_excluded(&self, path: &str) -> bool {
        self.hard.iter().any(|p| p.matches(path))
    }

    pub fn is_excluded(&self, path: &str) -> bool {
        if self.is_hard_excluded(path) {
            return true;
        }
        let Some(pos) = self.rules.iter().rposition(|r| r.pattern.matches(path)) else {
            return false;
        };
        if self.rules[pos].include {
            return false;
        }
        // Исключённый каталог с повторно включённым внутри остаётся видимым.
        !self.rules[pos + 1..]
            .iter()
            .any(|r| r.include && r.pattern.may_match_inside(path))
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

/// Шаблон может продолжиться за последним сегментом `segs` (совпасть с путём
/// внутри каталога `segs`).
fn glob_extends(pat: &[String], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => false,
        Some((first, _)) if first == "**" => true,
        Some((first, rest)) => match segs.split_first() {
            None => true,
            Some((s, srest)) => glob_segment(first, s) && glob_extends(rest, srest),
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
    fn reinclude_last_rule_wins() {
        let e = Excludes::new(&[
            ".obsidian/",
            "!.obsidian/hotkeys.json",
            "!.obsidian/themes/**",
            "!.obsidian/plugins/*/main.js",
            ".obsidian/themes/broken/",
        ]);
        assert!(!e.is_excluded(".obsidian/hotkeys.json"));
        assert!(e.is_excluded(".obsidian/app.json"));
        assert!(!e.is_excluded(".obsidian/themes/Minimal/theme.css"));
        assert!(
            e.is_excluded(".obsidian/themes/broken/theme.css"),
            "последнее правило"
        );
        assert!(!e.is_excluded(".obsidian/plugins/dataview/main.js"));
        assert!(e.is_excluded(".obsidian/plugins/dataview/data.json"));
        // каталоги на пути к включённому не исключены (иначе их «исчезновение»
        // унесло бы и включённое)
        assert!(!e.is_excluded(".obsidian"));
        assert!(!e.is_excluded(".obsidian/plugins"));
        assert!(!e.is_excluded(".obsidian/plugins/dataview"));
        assert!(e.is_excluded(".obsidian/snippets"));
        // включение без исключения ничего не меняет
        assert!(!e.is_excluded("notes/a.md"));
    }

    #[test]
    fn exclude_after_include_wins() {
        let e = Excludes::new(&["!a.md", "a.md", "!b/**", "b/c.md"]);
        assert!(e.is_excluded("a.md"));
        assert!(e.is_excluded("b/c.md"));
        assert!(!e.is_excluded("b/d.md"));
    }

    #[test]
    fn hard_excludes_cannot_be_reincluded() {
        let mut e = Excludes::new(&[".obsidian/", "!.obsidian/plugins/**", "!.trash/"]);
        e.push_hard(".obsidian/plugins/selfsync/");
        e.push_hard(".trash/");
        assert!(e.is_excluded(".obsidian/plugins/selfsync/data.json"));
        assert!(e.is_excluded(".obsidian/plugins/selfsync"));
        assert!(!e.is_excluded(".obsidian/plugins/dataview/data.json"));
        assert!(e.is_excluded(".trash/x.md"));
        assert!(e.is_hard_excluded(".trash"));
        assert!(!e.is_hard_excluded(".obsidian/plugins/dataview"));
    }

    #[test]
    fn name_reinclude() {
        let e = Excludes::new(&["private/", "!*.md"]);
        assert!(!e.is_excluded("private/a.md"));
        assert!(e.is_excluded("private/a.png"));
        // Каталог исключён, файлы внутри приходят без записи папки: исполнители
        // создают родительские каталоги сами.
        assert!(e.is_excluded("private"));
    }

    #[test]
    fn segment_glob() {
        assert!(glob_segment("a*b*c", "aXXbYYc"));
        assert!(!glob_segment("a*b*c", "aXXbYY"));
        assert!(glob_segment("*", ""));
        assert!(glob_segment("*.md", ".md"));
    }
}
