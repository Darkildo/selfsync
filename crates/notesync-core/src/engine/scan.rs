//! Сканирование локального vault'а: какие файлы изменились, появились, исчезли.
//!
//! Полный обход — при старте, при возврате приложения на передний план и не реже
//! `full_scan_ms`; в остальное время перечитываются только пути из событий. Файл
//! перехэшируется, только если изменились размер или mtime.

use std::collections::{BTreeMap, BTreeSet};

use super::ctx::{Ctx, SyncResult};
use super::transfer::read_local;
use super::types::{FileMeta, LogLevel, Notice};
use crate::hash::Hash;
use crate::index::{FileState, LocalObs};
use crate::path::VaultPath;

const DIR_OBS: LocalObs = LocalObs {
    size: 0,
    mtime: 0,
    plain: Hash([0; 32]),
};

#[derive(Default)]
struct ScanDelta {
    /// Пути, появившиеся в индексе при этом сканировании.
    new: Vec<String>,
    /// Пути, исчезнувшие локально (были чистыми копиями базы).
    gone: Vec<String>,
}

/// Нормализует путь из ФС. Если имя на диске не в NFC, переименовывает файл.
async fn canonical(cx: &Ctx, raw: &str, is_dir: bool) -> SyncResult<Option<String>> {
    let Ok(vp) = VaultPath::normalize(raw) else {
        let already = cx.with_mut(|s| !s.notified.insert(raw.to_owned()));
        if !already {
            cx.log(LogLevel::Warn, format!("имя не синхронизируется: {raw}"));
            cx.notify(Notice::Rejected {
                path: raw.to_owned(),
                code: VaultPath::normalize(raw)
                    .err()
                    .map_or("path_invalid", |e| e.code())
                    .to_owned(),
            });
        }
        return Ok(None);
    };
    let nfc = vp.as_str().to_owned();
    if cx.with(|s| s.is_excluded(&nfc)) {
        return Ok(None);
    }
    if nfc != raw.trim_matches('/') {
        // NFD с macOS или из архива: приводим имя на диске к NFC (через временное
        // имя — на нормализационно-нечувствительных ФС это «то же самое» имя).
        let mut target = nfc.clone();
        if cx.stat(&nfc).await?.is_some_and(|m| m.path != raw) {
            // На ФС, различающей нормализацию (Linux), это два разных файла: NFD-вариант
            // сохраняется под свободным NFC-именем, а не пропускается.
            let (date, _) = super::resolve::stamp(cx.now(), cx.with(|s| s.cfg.tz_offset_min));
            let label = format!("nfd {date}");
            target = super::resolve::unique_copy(cx, &nfc, &label).await?;
            cx.log(
                LogLevel::Warn,
                format!("есть и NFD, и NFC вариант имени: NFD-файл сохранён как {target}"),
            );
        }
        let tmp = format!("{target}.nfc{}", super::ctx::TEMP_SUFFIX);
        if cx.rename(raw, &tmp).await?.is_none() {
            return Ok(None);
        }
        if cx.rename(&tmp, &target).await?.is_none() {
            cx.rename(&tmp, raw).await?;
            return Ok(None);
        }
        let _ = is_dir;
        return Ok(Some(target));
    }
    Ok(Some(nfc))
}

/// Брошенный временный файл (процесс убит посреди операции): двухшаговое
/// переименование доводится до конца, недописанная атомарная запись уходит в корзину.
async fn recover_temp(cx: &Ctx, raw: &str) -> SyncResult<Option<String>> {
    let base = &raw[..raw.len() - super::ctx::TEMP_SUFFIX.len()];
    // Атомарная запись исполнителя без атомарной замены (мобильный adapter):
    // `X.commit.notesync-tmp` уже записан целиком. Если X нет — убит между удалением
    // X и переименованием: довести. Если X есть — до замены не дошло, а запись
    // повторит следующий цикл (индекс её ещё не учёл).
    if let Some(target) = base.strip_suffix(".commit") {
        if cx.stat(target).await?.is_none() {
            cx.log(
                LogLevel::Warn,
                format!("доведена прерванная запись: {target}"),
            );
            return Ok(cx.rename(raw, target).await?.map(|_| target.to_owned()));
        }
        cx.trash(raw, super::types::Expect::Any).await?;
        return Ok(None);
    }
    let target = base
        .strip_suffix(".case")
        .or_else(|| base.strip_suffix(".nfc"));
    let Some(target) = target else {
        cx.log(
            LogLevel::Warn,
            format!("брошенный временный файл убран в корзину: {raw}"),
        );
        cx.trash(raw, super::types::Expect::Any).await?;
        return Ok(None);
    };
    let dest = if cx.stat(target).await?.is_none() {
        target.to_owned()
    } else {
        let (date, _) = super::resolve::stamp(cx.now(), cx.with(|s| s.cfg.tz_offset_min));
        super::resolve::unique_copy(cx, target, &format!("recovered {date}")).await?
    };
    cx.log(
        LogLevel::Warn,
        format!("доведено прерванное переименование: {raw} → {dest}"),
    );
    Ok(cx.rename(raw, &dest).await?.map(|_| dest))
}

/// Вносит в индекс переименования из событий Obsidian.
fn apply_rename_events(cx: &Ctx) {
    cx.with_mut(|s| {
        let renames = std::mem::take(&mut s.renames);
        for (from_raw, to_raw) in renames {
            let from = VaultPath::normalize(&from_raw).map(|p| p.as_str().to_owned());
            let to = VaultPath::normalize(&to_raw).map(|p| p.as_str().to_owned());
            let (Ok(from), Ok(to)) = (from, to) else {
                s.need_full_scan = true;
                continue;
            };
            let ex_from = s.is_excluded(&from);
            let ex_to = s.is_excluded(&to);
            if ex_from || ex_to {
                if !ex_from {
                    s.dirty.insert(from);
                }
                if !ex_to {
                    s.dirty.insert(to);
                }
                continue;
            }
            if s.index.files.contains_key(&to) || !s.index.files.contains_key(&from) {
                s.dirty.insert(from);
                s.dirty.insert(to);
                continue;
            }
            // Сам путь и (для папки) всё внутри.
            let from_vp = VaultPath::parse(&from).ok();
            let to_vp = VaultPath::parse(&to).ok();
            let keys: Vec<String> = s
                .index
                .files
                .keys()
                .filter(|k| {
                    **k == from
                        || match (&from_vp, VaultPath::parse(k).ok()) {
                            (Some(f), Some(kp)) => kp.is_inside(f),
                            _ => false,
                        }
                })
                .cloned()
                .collect();
            for k in keys {
                let new_key = match (&from_vp, &to_vp, VaultPath::parse(&k).ok()) {
                    (Some(f), Some(t), Some(kp)) => kp.rebase(f, t).map(|p| p.as_str().to_owned()),
                    _ => None,
                };
                let Some(new_key) = new_key else { continue };
                if s.index.files.contains_key(&new_key) {
                    s.dirty.insert(k);
                    s.dirty.insert(new_key);
                    continue;
                }
                let Some(mut f) = s.index.files.remove(&k) else {
                    continue;
                };
                // Файл есть на сервере, если он когда-либо синхронизировался (даже если
                // ревизия сейчас неизвестна после сброса баз).
                if f.server_path.is_none() && f.maybe_on_server() {
                    f.server_path = Some(k.clone());
                }
                if f.server_path.as_deref() == Some(new_key.as_str()) {
                    f.server_path = None;
                }
                s.dirty.insert(new_key.clone());
                s.index.files.insert(new_key, f);
            }
        }
    });
}

/// Обновляет наблюдение одного пути.
async fn observe(cx: &Ctx, path: &str, meta: &FileMeta, delta: &mut ScanDelta) -> SyncResult<()> {
    if meta.dir {
        cx.with_mut(|s| {
            let e = s.index.files.entry(path.to_owned()).or_insert_with(|| {
                delta.new.push(path.to_owned());
                FileState {
                    folder: true,
                    ..Default::default()
                }
            });
            e.folder = true;
            e.local = Some(DIR_OBS);
        });
        return Ok(());
    }
    let (known, unchanged, max) = cx.with(|s| {
        let f = s.index.files.get(path);
        let unchanged = f
            .and_then(|f| f.local.as_ref())
            .is_some_and(|l| l.size == meta.size && l.mtime == meta.mtime);
        (f.is_some(), unchanged, s.cfg.max_file_size)
    });
    if unchanged {
        return Ok(());
    }
    if meta.size > max {
        let first = cx.with_mut(|s| {
            let f = s.index.files.entry(path.to_owned()).or_default();
            let first = f.rejected.as_ref().is_none_or(|(c, _)| c != "too_large");
            f.local = Some(LocalObs {
                size: meta.size,
                mtime: meta.mtime,
                plain: Hash::default(),
            });
            f.rejected = Some(("too_large".into(), Hash::default()));
            first
        });
        if first {
            cx.notify(Notice::TooLarge {
                path: path.to_owned(),
                size: meta.size,
            });
        }
        return Ok(());
    }
    let Some(content) = read_local(cx, path, meta).await? else {
        mark_gone(cx, path, delta);
        return Ok(());
    };
    cx.with_mut(|s| {
        let f = s.index.files.entry(path.to_owned()).or_default();
        f.folder = false;
        f.local = Some(content.obs);
        if f.rejected.as_ref().is_some_and(|(c, _)| c == "too_large") {
            f.rejected = None;
        }
    });
    if !known {
        delta.new.push(path.to_owned());
    }
    Ok(())
}

fn mark_gone(cx: &Ctx, path: &str, delta: &mut ScanDelta) {
    cx.with_mut(|s| {
        let vp = VaultPath::parse(path).ok();
        let inside: Vec<String> = match &vp {
            Some(dir) => s
                .index
                .files
                .keys()
                .filter(|k| VaultPath::parse(k).is_ok_and(|kp| kp.is_inside(dir)))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        for k in std::iter::once(path.to_owned()).chain(inside) {
            let Some(f) = s.index.files.get_mut(&k) else {
                continue;
            };
            if f.local.is_none() {
                continue;
            }
            let was_clean = f.clean();
            f.local = None;
            if !f.maybe_on_server() && f.server_path.is_none() {
                // Никогда не было на сервере — удалять нечего.
                if f.transfer.is_none() {
                    s.index.files.remove(&k);
                }
            } else if was_clean && !f.folder {
                delta.gone.push(k);
            }
        }
    });
}

/// Распознаёт переименование «удалён + появился с тем же содержимым».
fn detect_renames(cx: &Ctx, delta: &ScanDelta) {
    if delta.new.is_empty() || delta.gone.is_empty() {
        return;
    }
    cx.with_mut(|s| {
        let mut gone_by_hash: BTreeMap<Hash, Vec<String>> = BTreeMap::new();
        for g in &delta.gone {
            if let Some(h) = s.index.files.get(g).and_then(|f| f.base_plain) {
                gone_by_hash.entry(h).or_default().push(g.clone());
            }
        }
        let mut new_by_hash: BTreeMap<Hash, Vec<String>> = BTreeMap::new();
        for n in &delta.new {
            if let Some(f) = s.index.files.get(n)
                && let (Some(l), false, 0) = (&f.local, f.folder, f.base_rev)
            {
                new_by_hash.entry(l.plain).or_default().push(n.clone());
            }
        }
        for (h, gone) in gone_by_hash {
            let Some(news) = new_by_hash.get(&h) else {
                continue;
            };
            if gone.len() != 1 || news.len() != 1 {
                continue; // неоднозначно — пусть будет удаление + новый файл
            }
            let (old, new) = (&gone[0], &news[0]);
            let Some(mut f) = s.index.files.remove(old) else {
                continue;
            };
            let local = s.index.files.get(new).and_then(|n| n.local);
            f.server_path = f.server_path.take().or_else(|| Some(old.clone()));
            if f.server_path.as_deref() == Some(new.as_str()) {
                f.server_path = None;
            }
            f.local = local;
            s.index.files.insert(new.clone(), f);
        }
    });
}

/// Сканирование: полный обход или пути из событий.
pub(crate) async fn scan(cx: &Ctx) -> SyncResult<()> {
    let ev = cx.with(|s| s.renames.clone());
    if !ev.is_empty() {
        cx.log(LogLevel::Debug, format!("rename events {ev:?}"));
    }
    apply_rename_events(cx);
    let full = cx.with(|s| {
        s.need_full_scan
            || s.last_full_scan == 0
            || s.now - s.last_full_scan >= i64::try_from(s.cfg.full_scan_ms).unwrap_or(i64::MAX)
    });
    let mut delta = ScanDelta::default();
    if full {
        let listing = cx.list().await?;
        cx.with_mut(|s| {
            s.need_full_scan = false;
            s.last_full_scan = s.now;
            s.dirty.clear();
        });
        let mut present: BTreeMap<String, FileMeta> = BTreeMap::new();
        for mut m in listing {
            if m.path.ends_with(super::ctx::TEMP_SUFFIX)
                && !cx.with(|s| s.excludes.is_excluded(&m.path))
            {
                match recover_temp(cx, &m.path).await? {
                    Some(p) => m.path = p,
                    None => continue,
                }
            }
            if let Some(p) = canonical(cx, &m.path, m.dir).await? {
                present.insert(p, m);
            }
        }
        for (p, m) in &present {
            observe(cx, p, m, &mut delta).await?;
        }
        let tracked: Vec<String> = cx.with(|s| {
            s.index
                .files
                .iter()
                .filter(|(_, f)| f.local.is_some())
                .map(|(k, _)| k.clone())
                .collect()
        });
        for p in tracked {
            if !present.contains_key(&p) {
                mark_gone(cx, &p, &mut delta);
            }
        }
    } else {
        let dirty: BTreeSet<String> = cx.with_mut(|s| std::mem::take(&mut s.dirty));
        for raw in dirty {
            match cx.stat(&raw).await? {
                Some(m) => {
                    // Регистронезависимая ФС: по этому имени нашёлся файл с другим
                    // регистром — запрошенного пути нет, наблюдаем настоящее имя.
                    let actual = if m.path.is_empty() {
                        raw.clone()
                    } else {
                        m.path.clone()
                    };
                    if actual != raw
                        && let Ok(vp) = VaultPath::normalize(&raw)
                    {
                        mark_gone(cx, vp.as_str(), &mut delta);
                    }
                    if let Some(p) = canonical(cx, &actual, m.dir).await? {
                        observe(cx, &p, &m, &mut delta).await?;
                    }
                }
                None => {
                    if let Ok(vp) = VaultPath::normalize(&raw) {
                        mark_gone(cx, vp.as_str(), &mut delta);
                    }
                }
            }
        }
    }
    if !delta.new.is_empty() || !delta.gone.is_empty() {
        cx.log(
            LogLevel::Debug,
            format!(
                "scan full={full}: new {:?}, gone {:?}",
                delta.new, delta.gone
            ),
        );
    }
    detect_renames(cx, &delta);
    cx.save().await
}
