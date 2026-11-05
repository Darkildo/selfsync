//! Фейковая файловая система клиента: vault, корзина, временные файлы, кэш.

use std::collections::BTreeMap;

use selfsync_core::engine::{Expect, FileMeta, IoResult};
use selfsync_core::path::{MAX_SEGMENT_BYTES, NameRules, VaultPath};
use selfsync_core::tempname;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File {
        data: Vec<u8>,
        mtime: i64,
        /// Время изменения inode: меняется при каждой записи, даже если mtime потом
        /// восстановлен (`user_write_keep_mtime`).
        ctime: i64,
    },
    Dir,
    /// Символическая ссылка за пределы vault'а: что за ней — не видно.
    Link,
}

/// ФС vault'а. На регистронезависимой ФС ключ — путь в нижнем регистре, а
/// отображаемое имя хранится отдельно (как в NTFS/APFS).
#[derive(Debug, Clone, Default)]
pub struct FakeFs {
    pub case_insensitive: bool,
    /// Какие имена ФС может хранить (Android, Windows): остальные не создаются.
    pub name_rules: NameRules,
    nodes: BTreeMap<String, (String, Node)>,
    /// Корзина: всё, что убрано движком (Trash), с версиями.
    pub trash: Vec<(String, Vec<u8>)>,
    pub temps: BTreeMap<String, Vec<u8>>,
    pub cache: BTreeMap<String, Vec<u8>>,
    /// Попытки изменить что-то через символическую ссылку (за пределами vault'а).
    pub link_writes: u32,
    /// ФС не сообщает ctime (мобильные): правку с восстановленным mtime видно
    /// только по содержимому.
    pub no_ctime: bool,
    /// Единица mtime, мс (FAT — 2000): mtime округляется вниз до неё, и правка в
    /// ту же единицу времени оставляет его прежним. 0 — точный mtime.
    pub mtime_granularity: i64,
    /// Файлы, которые адаптер не может прочитать (мобильный: `stat` отвечает «нет
    /// файла», хотя листинг папки его показывает).
    pub unreadable: std::collections::BTreeSet<String>,
    clock: i64,
}

fn parent(p: &str) -> Option<&str> {
    p.rfind('/').map(|i| &p[..i])
}

impl FakeFs {
    pub fn new(case_insensitive: bool, name_rules: NameRules) -> FakeFs {
        FakeFs {
            case_insensitive,
            name_rules,
            ..Default::default()
        }
    }

    /// Такое имя здесь не создать (`?` на Android и т.п.; имя длиннее 255 байт —
    /// ENAMETOOLONG на любой ФС, в том числе временное имя с суффиксом).
    pub fn forbids(&self, p: &str) -> bool {
        p.split('/').any(|seg| seg.len() > MAX_SEGMENT_BYTES)
            || VaultPath::normalize(p).is_ok_and(|v| self.name_rules.rejects(&v).is_some())
    }

    fn bad_name() -> IoResult {
        IoResult::Failed {
            message: "invalid file name".into(),
        }
    }

    fn key(&self, p: &str) -> String {
        if self.case_insensitive {
            p.to_lowercase()
        } else {
            p.to_owned()
        }
    }

    /// mtime: монотонный счётчик поверх часов клиента (разрешение mtime не мешает
    /// замечать правки).
    fn tick(&mut self, now: i64) -> i64 {
        self.clock = (self.clock + 1).max(now);
        self.clock
    }

    pub fn get(&self, p: &str) -> Option<&Node> {
        self.nodes.get(&self.key(p)).map(|(_, n)| n)
    }

    pub fn display_name(&self, p: &str) -> Option<&str> {
        self.nodes.get(&self.key(p)).map(|(n, _)| n.as_str())
    }

    pub fn read_file(&self, p: &str) -> Option<&Vec<u8>> {
        match self.get(p) {
            Some(Node::File { data, .. }) => Some(data),
            _ => None,
        }
    }

    /// Путь — ссылка или лежит под ней.
    pub fn link_on(&self, p: &str) -> bool {
        let mut cur = p;
        loop {
            if matches!(self.get(cur), Some(Node::Link)) {
                return true;
            }
            match parent(cur) {
                Some(par) => cur = par,
                None => return false,
            }
        }
    }

    /// Изменение через ссылку: учесть попытку и отказать.
    fn through_link(&mut self, paths: &[&str]) -> Option<IoResult> {
        if paths.iter().any(|p| self.link_on(p)) {
            self.link_writes += 1;
            return Some(IoResult::Failed {
                message: "path goes through a symbolic link".into(),
            });
        }
        None
    }

    /// Пользователь заменил папку ссылкой на папку за пределами vault'а (перенёс
    /// её наружу и оставил ссылку): содержимое из vault'а пропало, но не удалено.
    pub fn user_link(&mut self, p: &str) {
        let k = self.key(p);
        let prefix = format!("{k}/");
        self.nodes.retain(|x, _| *x != k && !x.starts_with(&prefix));
        self.ensure_parents(p);
        self.nodes.insert(k, (p.to_owned(), Node::Link));
    }

    /// Ссылку убрали.
    pub fn user_unlink(&mut self, p: &str) {
        let k = self.key(p);
        if matches!(self.get(p), Some(Node::Link)) {
            self.nodes.remove(&k);
        }
    }

    fn ensure_parents(&mut self, p: &str) {
        let mut cur = p;
        let mut chain = Vec::new();
        while let Some(par) = parent(cur) {
            chain.push(par.to_owned());
            cur = par;
        }
        for d in chain.into_iter().rev() {
            let k = self.key(&d);
            self.nodes.entry(k).or_insert((d, Node::Dir));
        }
    }

    /// Запись пользователем (без условий).
    pub fn user_write(&mut self, p: &str, data: Vec<u8>, now: i64) {
        self.ensure_parents(p);
        let ctime = self.tick(now);
        let mtime = if self.mtime_granularity > 0 {
            ctime - ctime.rem_euclid(self.mtime_granularity)
        } else {
            ctime
        };
        let k = self.key(p);
        let name = self
            .nodes
            .get(&k)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| p.to_owned());
        self.nodes
            .insert(k, (name, Node::File { data, mtime, ctime }));
    }

    /// Пользователь (`cp -p`, `rsync -t`) меняет содержимое и восстанавливает прежний
    /// mtime: размер и mtime те же, сменился только ctime.
    pub fn user_write_keep_mtime(&mut self, p: &str, data: Vec<u8>, now: i64) {
        let ctime = self.tick(now);
        let k = self.key(p);
        if let Some((
            _,
            Node::File {
                data: d, ctime: c, ..
            },
        )) = self.nodes.get_mut(&k)
        {
            *d = data;
            *c = ctime;
        }
    }

    /// Атомарная запись, во время которой (между проверкой условия и заменой)
    /// пользователь сохранил в файл `edit`: новый файл на месте, а версия
    /// пользователя отодвинута под временное имя (`IoResult::Displaced`).
    pub fn displace_write(
        &mut self,
        p: &str,
        data: Vec<u8>,
        expect: &Expect,
        edit: Vec<u8>,
        now: i64,
    ) -> IoResult {
        if self.link_on(p) || self.forbids(p) || !self.check(p, expect) {
            return self.write(p, data, expect, now);
        }
        if matches!(self.get(p), Some(Node::Dir)) {
            return IoResult::Precondition;
        }
        let aside = tempname::aside(p, 1);
        self.user_write(&aside, edit, now);
        self.user_write(p, data, now);
        match self.stat(p) {
            IoResult::Stat { meta: Some(meta) } => IoResult::Displaced { meta, aside },
            other => other,
        }
    }

    pub fn user_mkdir(&mut self, p: &str) {
        self.ensure_parents(p);
        let k = self.key(p);
        self.nodes.entry(k).or_insert((p.to_owned(), Node::Dir));
    }

    /// Удаление пользователем (файл или папка с содержимым).
    pub fn user_delete(&mut self, p: &str) -> Vec<String> {
        let k = self.key(p);
        let prefix = format!("{k}/");
        let victims: Vec<String> = self
            .nodes
            .keys()
            .filter(|x| **x == k || x.starts_with(&prefix))
            .cloned()
            .collect();
        let mut names = Vec::new();
        for v in victims {
            if let Some((n, node)) = self.nodes.remove(&v) {
                // Как в Obsidian: удалённое пользователем уходит в корзину.
                if let Node::File { data, .. } = node {
                    self.trash.push((n.clone(), data));
                }
                names.push(n);
            }
        }
        names
    }

    /// Переименование пользователем. `false` — назначение занято.
    pub fn user_rename(&mut self, from: &str, to: &str, now: i64) -> bool {
        self.rename(from, to, now)
    }

    fn rename(&mut self, from: &str, to: &str, _now: i64) -> bool {
        let fk = self.key(from);
        let tk = self.key(to);
        if !self.nodes.contains_key(&fk) {
            return false;
        }
        if fk != tk && self.nodes.contains_key(&tk) {
            return false;
        }
        self.ensure_parents(to);
        let prefix = format!("{fk}/");
        let moved: Vec<String> = self
            .nodes
            .keys()
            .filter(|x| **x == fk || x.starts_with(&prefix))
            .cloned()
            .collect();
        let mut items = Vec::new();
        for k in moved {
            if let Some(v) = self.nodes.remove(&k) {
                items.push((k, v));
            }
        }
        for (k, (name, node)) in items {
            let rest = &name[from.len().min(name.len())..];
            let new_name = format!("{to}{rest}");
            let new_key = format!("{tk}{}", &k[fk.len()..]);
            self.nodes.insert(new_key, (new_name, node));
        }
        true
    }

    /// Все видимые файлы vault'а: путь → содержимое.
    pub fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        self.nodes
            .values()
            .filter_map(|(n, node)| match node {
                Node::File { data, .. } => Some((n.clone(), data.clone())),
                Node::Dir | Node::Link => None,
            })
            .collect()
    }

    pub fn dirs(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|(_, n)| matches!(n, Node::Dir))
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn meta_of(&self, name: &str, node: &Node) -> FileMeta {
        let mut m = Self::meta(name, node);
        if self.no_ctime {
            m.ctime = 0;
        }
        m
    }

    fn meta(name: &str, node: &Node) -> FileMeta {
        match node {
            Node::File { data, mtime, ctime } => FileMeta {
                path: name.to_owned(),
                size: data.len() as u64,
                mtime: *mtime,
                dir: false,
                link: false,
                ctime: *ctime,
                unreadable: false,
            },
            Node::Dir => FileMeta {
                path: name.to_owned(),
                size: 0,
                mtime: 0,
                dir: true,
                link: false,
                ctime: 0,
                unreadable: false,
            },
            Node::Link => FileMeta {
                path: name.to_owned(),
                size: 0,
                mtime: 0,
                dir: false,
                link: true,
                ctime: 0,
                unreadable: false,
            },
        }
    }

    fn check(&self, p: &str, e: &Expect) -> bool {
        match (e, self.get(p)) {
            (Expect::Any, _) => true,
            (Expect::Absent, None) => true,
            (Expect::Absent, Some(_)) => false,
            (Expect::Stat { size, mtime }, Some(Node::File { data, mtime: m, .. })) => {
                data.len() as u64 == *size && m == mtime
            }
            (Expect::Stat { .. }, _) => false,
        }
    }

    pub fn list(&self) -> IoResult {
        IoResult::Listing {
            files: self
                .nodes
                .values()
                .map(|(n, node)| {
                    let mut m = self.meta_of(n, node);
                    if self.unreadable.contains(n) {
                        m = FileMeta {
                            path: n.clone(),
                            unreadable: true,
                            ..FileMeta::default()
                        };
                    }
                    m
                })
                .collect(),
        }
    }

    pub fn stat(&self, p: &str) -> IoResult {
        if self.unreadable.contains(p) {
            return IoResult::Stat { meta: None };
        }
        if self.link_on(p) {
            return IoResult::Stat {
                meta: Some(Self::meta(p, &Node::Link)),
            };
        }
        match self.nodes.get(&self.key(p)) {
            Some((n, node)) => IoResult::Stat {
                meta: Some(self.meta_of(n, node)),
            },
            None => IoResult::Stat { meta: None },
        }
    }

    pub fn read(&self, p: &str, offset: u64, len: Option<u64>) -> IoResult {
        if self.link_on(p) || self.unreadable.contains(p) {
            return IoResult::NotFound;
        }
        match self.get(p) {
            Some(Node::File { data, .. }) => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                let end = match len {
                    Some(l) => start
                        .saturating_add(usize::try_from(l).unwrap_or(usize::MAX))
                        .min(data.len()),
                    None => data.len(),
                };
                IoResult::Data {
                    data: data[start..end].to_vec(),
                }
            }
            Some(Node::Dir) => IoResult::Failed {
                message: "is a directory".into(),
            },
            Some(Node::Link) | None => IoResult::NotFound,
        }
    }

    pub fn write(&mut self, p: &str, data: Vec<u8>, expect: &Expect, now: i64) -> IoResult {
        if let Some(r) = self.through_link(&[p]) {
            return r;
        }
        if self.forbids(p) {
            return Self::bad_name();
        }
        if !self.check(p, expect) {
            return IoResult::Precondition;
        }
        if matches!(self.get(p), Some(Node::Dir)) {
            return IoResult::Precondition;
        }
        self.user_write(p, data, now);
        self.stat(p)
    }

    pub fn write_temp(&mut self, t: &str, offset: u64, data: &[u8]) -> IoResult {
        let buf = self.temps.entry(t.to_owned()).or_default();
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        if buf.len() < off {
            return IoResult::Failed {
                message: "gap in temp file".into(),
            };
        }
        buf.truncate(off);
        buf.extend_from_slice(data);
        IoResult::Done
    }

    pub fn read_temp(&self, t: &str, offset: u64, len: u64) -> IoResult {
        match self.temps.get(t) {
            Some(d) => {
                let s = usize::try_from(offset).unwrap_or(usize::MAX).min(d.len());
                let e = s
                    .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
                    .min(d.len());
                IoResult::Data {
                    data: d[s..e].to_vec(),
                }
            }
            None => IoResult::NotFound,
        }
    }

    pub fn commit_temp(&mut self, t: &str, p: &str, expect: &Expect, now: i64) -> IoResult {
        if let Some(r) = self.through_link(&[p]) {
            return r;
        }
        if self.forbids(p) {
            return Self::bad_name();
        }
        if !self.check(p, expect) {
            return IoResult::Precondition;
        }
        let Some(data) = self.temps.remove(t) else {
            return IoResult::NotFound;
        };
        self.user_write(p, data, now);
        self.stat(p)
    }

    /// Удалить насовсем (освобождение места под исключённым путём).
    pub fn delete(&mut self, p: &str, expect: &Expect) -> IoResult {
        if let Some(r) = self.through_link(&[p]) {
            return r;
        }
        if self.get(p).is_none() {
            return IoResult::NotFound;
        }
        if !self.check(p, expect) {
            return IoResult::Precondition;
        }
        match self.get(p) {
            Some(Node::File { .. }) => {
                self.nodes.remove(&self.key(p));
                IoResult::Done
            }
            _ => IoResult::Precondition,
        }
    }

    pub fn trash(&mut self, p: &str, expect: &Expect) -> IoResult {
        if let Some(r) = self.through_link(&[p]) {
            return r;
        }
        if self.get(p).is_none() {
            return IoResult::NotFound;
        }
        if !self.check(p, expect) {
            return IoResult::Precondition;
        }
        match self.get(p).cloned() {
            Some(Node::File { data, .. }) => {
                let name = self.display_name(p).unwrap_or(p).to_owned();
                self.nodes.remove(&self.key(p));
                self.trash.push((name, data));
                IoResult::Done
            }
            Some(Node::Dir | Node::Link) => IoResult::Precondition,
            None => IoResult::NotFound,
        }
    }

    pub fn rename_io(&mut self, from: &str, to: &str, now: i64) -> IoResult {
        if let Some(r) = self.through_link(&[from, to]) {
            return r;
        }
        if self.get(from).is_none() {
            return IoResult::NotFound;
        }
        if self.forbids(to) {
            return Self::bad_name();
        }
        if self.rename(from, to, now) {
            self.stat(to)
        } else {
            IoResult::Precondition
        }
    }

    pub fn mkdir(&mut self, p: &str) -> IoResult {
        if let Some(r) = self.through_link(&[p]) {
            return r;
        }
        if self.forbids(p) {
            return Self::bad_name();
        }
        if matches!(self.get(p), Some(Node::File { .. })) {
            return IoResult::Failed {
                message: "file exists".into(),
            };
        }
        self.user_mkdir(p);
        IoResult::Done
    }

    pub fn rmdir(&mut self, p: &str) -> IoResult {
        if let Some(r) = self.through_link(&[p]) {
            return r;
        }
        let k = self.key(p);
        let prefix = format!("{k}/");
        if !self.nodes.contains_key(&k) {
            return IoResult::NotFound;
        }
        if self.nodes.keys().any(|x| x.starts_with(&prefix)) {
            return IoResult::Precondition;
        }
        self.nodes.remove(&k);
        IoResult::Done
    }
}
