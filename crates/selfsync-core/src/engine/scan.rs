//! Сканирование локального vault'а: какие файлы изменились, появились, исчезли.
//!
//! Полный обход — при старте, при возврате приложения на передний план и не реже
//! `full_scan_ms`; в остальное время перечитываются только пути из событий. Файл
//! перехэшируется, только если изменились размер или mtime.

use std::collections::{BTreeMap, BTreeSet};

use super::ctx::{Ctx, RACY_MS, SMALL_BLOB, SyncError, SyncResult};
use super::transfer::read_local;
use super::types::{FileMeta, LogLevel, Notice};
use crate::hash::Hash;
use crate::index::{FileState, LocalObs};
use crate::path::VaultPath;
use crate::tempname;

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
        let _ = is_dir;
        if !cx.rename_via_temp(raw, &target, "nfc").await? {
            return Ok(None);
        }
        return Ok(Some(target));
    }
    Ok(Some(nfc))
}

/// Брошенный временный файл (процесс убит посреди операции): двухшаговое
/// переименование доводится до конца, недописанная атомарная запись уходит в корзину.
async fn recover_temp(cx: &Ctx, raw: &str) -> SyncResult<Option<String>> {
    let base = &raw[..raw.len() - super::ctx::TEMP_SUFFIX.len()];
    // Атомарная запись исполнителя без атомарной замены (мобильный adapter):
    // `X.commit.selfsync-tmp` уже записан целиком. Если X нет — убит между удалением
    // X и переименованием: довести. Если X есть — до замены не дошло, а запись
    // повторит следующий цикл (индекс её ещё не учёл).
    if let Some(stem) = base.strip_suffix(".commit") {
        // Укороченное имя, чей путь не в индексе: файл скачивался впервые, и
        // следующий цикл скачает его снова.
        let Some(target) = target_of(cx, stem) else {
            return trash_abandoned(cx, raw).await;
        };
        if cx.stat(&target).await?.is_none() {
            cx.log(
                LogLevel::Warn,
                format!("доведена прерванная запись: {target}"),
            );
            return Ok(cx.rename(raw, &target).await?.map(|_| target));
        }
        cx.trash(raw, super::types::Expect::Any).await?;
        return Ok(None);
    }
    // Вытесненная при атомарной записи версия (`IoResult::Displaced`): процесс убит
    // до того, как она стала копией конфликта.
    if let Some(stem) = strip_tag(base, ".aside") {
        return recover_aside(cx, raw, stem).await;
    }
    let Some(stem) = strip_tag(base, ".case").or_else(|| strip_tag(base, ".nfc")) else {
        return trash_abandoned(cx, raw).await;
    };
    // Укороченное имя без пути в индексе: файл остаётся под укороченным именем —
    // лучше так, чем потерять его содержимое.
    let target = target_of(cx, stem).unwrap_or_else(|| stem.to_owned());
    let dest = if cx.stat(&target).await?.is_none() {
        target
    } else {
        let (date, _) = super::resolve::stamp(cx.now(), cx.with(|s| s.cfg.tz_offset_min));
        super::resolve::unique_copy(cx, &target, &format!("recovered {date}")).await?
    };
    cx.log(
        LogLevel::Warn,
        format!("доведено прерванное переименование: {raw} → {dest}"),
    );
    Ok(cx.rename(raw, &dest).await?.map(|_| dest))
}

/// Брошенная вытесненная версия `X.aside.selfsync-tmp`: прежнее содержимое X,
/// отодвинутое атомарной записью. Совпадает с тем, что сейчас в X или в индексе
/// (запись не успела ничего вытеснить, или вытеснила известную версию), — лишняя
/// и удаляется; иначе это несохранённая правка — копия конфликта рядом с X.
async fn recover_aside(cx: &Ctx, raw: &str, stem: &str) -> SyncResult<Option<String>> {
    let Some(meta) = cx.stat(raw).await? else {
        return Ok(None);
    };
    let Some(aside) = read_local(cx, raw, &meta).await? else {
        return Ok(None);
    };
    let target = target_of(cx, stem).unwrap_or_else(|| stem.to_owned());
    let known = cx.with(|s| {
        s.index.files.get(&target).is_some_and(|f| {
            f.local.map(|l| l.plain) == Some(aside.obs.plain)
                || f.base_plain == Some(aside.obs.plain)
        })
    });
    let same_as_target = match cx.stat(&target).await? {
        Some(m) if !m.dir => read_local(cx, &target, &m)
            .await?
            .is_some_and(|c| c.obs.plain == aside.obs.plain),
        _ => false,
    };
    if known || same_as_target {
        cx.log(
            LogLevel::Info,
            format!("убрана лишняя вытесненная версия: {raw}"),
        );
        cx.delete(raw, super::types::Expect::Any).await?;
        return Ok(None);
    }
    let copy = super::resolve::unique_copy(cx, &target, &super::resolve::conflict_label(cx, false))
        .await?;
    cx.log(
        LogLevel::Warn,
        format!("вытесненная версия {target} сохранена как {copy}"),
    );
    if cx.rename(raw, &copy).await?.is_none() {
        return Ok(None);
    }
    super::resolve::add_conflict(cx, &target, &copy, false);
    Ok(Some(copy))
}

async fn trash_abandoned(cx: &Ctx, raw: &str) -> SyncResult<Option<String>> {
    cx.log(
        LogLevel::Warn,
        format!("брошенный временный файл убран в корзину: {raw}"),
    );
    cx.trash(raw, super::types::Expect::Any).await?;
    Ok(None)
}

/// Путь назначения по базе временного имени: сама база или, если она укорочена
/// (`tempname::temp_base`), путь из индекса в том же каталоге с тем же хэшем.
/// `None` — укороченная база, чей путь индексу не известен.
fn target_of(cx: &Ctx, base: &str) -> Option<String> {
    let Some(hash) = tempname::shortened_hash(base) else {
        return Some(base.to_owned());
    };
    let dir = tempname::dir_of(base);
    cx.with(|s| {
        s.index
            .files
            .keys()
            .find(|k| tempname::dir_of(k) == dir && tempname::path_hash(k) == hash)
            .cloned()
            // Своё имя файла, случайно похожее на укороченное.
            .or_else(|| s.index.files.contains_key(base).then(|| base.to_owned()))
    })
}

/// `X.tag` или `X.tag-N` (см. `Ctx::free_temp`) → `X`.
fn strip_tag<'a>(base: &'a str, tag: &str) -> Option<&'a str> {
    if let Some(t) = base.strip_suffix(tag) {
        return Some(t);
    }
    let (head, n) = base.rsplit_once('-')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    head.strip_suffix(tag)
}

/// Пути индекса внутри `from` (и сам `from`), чьё наблюдение совпадает с тем, что
/// сейчас лежит на диске: событие переименования пришло поздно, и синк успел вернуть
/// файл по старому пути (отменённое удаление, правка с другого устройства). Это уже не
/// тот файл, что переехал: его база и ревизия к новому имени не относятся.
async fn still_in_place(cx: &Ctx, from: &str, to: &str) -> SyncResult<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    // Смена регистра или нормализации: на ФС без различия это тот же путь.
    if from.to_lowercase() == to.to_lowercase() {
        return Ok(out);
    }
    let from_vp = VaultPath::parse(from).ok();
    let known: Vec<(String, Option<LocalObs>, bool)> = cx.with(|s| {
        s.index
            .files
            .iter()
            .filter(|(k, _)| {
                k.as_str() == from
                    || match (&from_vp, VaultPath::parse(k).ok()) {
                        (Some(f), Some(kp)) => kp.is_inside(f),
                        _ => false,
                    }
            })
            .map(|(k, f)| (k.clone(), f.local, f.folder))
            .collect()
    });
    for (k, local, folder) in known {
        let Some(local) = local else { continue };
        let Some(meta) = cx.stat(&k).await? else {
            continue;
        };
        let same = if folder {
            meta.dir
        } else {
            !meta.dir && meta.size == local.size && meta.mtime == local.mtime
        };
        if same {
            out.insert(k);
        }
    }
    Ok(out)
}

/// Вносит в индекс переименования из событий Obsidian.
async fn apply_rename_events(cx: &Ctx) -> SyncResult<()> {
    let pending = cx.with(|s| s.renames.clone());
    let mut in_place = BTreeSet::new();
    for (from_raw, to_raw) in &pending {
        let (Ok(from), Ok(to)) = (VaultPath::normalize(from_raw), VaultPath::normalize(to_raw))
        else {
            continue;
        };
        in_place.extend(still_in_place(cx, from.as_str(), to.as_str()).await?);
    }
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
                if s.index.files.contains_key(&new_key) || in_place.contains(&k) {
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
    Ok(())
}

/// Обновляет наблюдение одного пути. `verify` — не доверять размеру и mtime
/// (`UiCommand::VerifyContent`): файл хэшируется в любом случае.
async fn observe(
    cx: &Ctx,
    path: &str,
    meta: &FileMeta,
    verify: bool,
    delta: &mut ScanDelta,
) -> SyncResult<()> {
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
        // ctime не восстанавливается вместе с mtime: правка того же размера с
        // прежним mtime (`touch -r`, `cp -p`, `rsync -t`) видна по нему. Неизвестный
        // (мобильные, индекс старого формата) — не помеха.
        let same_ctime =
            meta.ctime == 0 || s.index.ctimes.get(path).is_none_or(|c| *c == meta.ctime);
        let unchanged = !verify
            && same_ctime
            && !s.racy.contains(path)
            && f.and_then(|f| f.local.as_ref())
                .is_some_and(|l| l.size == meta.size && l.mtime == meta.mtime);
        (f.is_some(), unchanged, s.max_upload())
    });
    if unchanged {
        // ctime ещё не запомнен (индекс старого формата): запомнить без перехэширования.
        note_ctime(cx, path, meta);
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
        if cx.stat(path).await?.is_some() {
            // Меняется прямо сейчас: перечитать в следующем цикле.
            cx.with_mut(|s| {
                s.dirty.insert(path.to_owned());
            });
        } else {
            mark_gone(cx, path, delta);
        }
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
    note_ctime(cx, path, meta);
    if !known {
        delta.new.push(path.to_owned());
    }
    Ok(())
}

/// Наблюдение `path` с этими размером и mtime сделано сейчас: «racily clean»,
/// если mtime ещё в пределах `RACY_MS` от текущего времени (см. `State::racy`).
/// Большие файлы не в счёт: перечитывать их ради такой маловероятной правки дорого.
pub(crate) fn note_racy(cx: &Ctx, path: &str, size: u64, mtime: i64) {
    cx.with_mut(|s| {
        if size <= SMALL_BLOB && mtime.saturating_add(RACY_MS) > s.now {
            s.racy.insert(path.to_owned());
        } else {
            s.racy.remove(path);
        }
    });
}

/// Запомнить ctime наблюдённого файла (см. `Index::ctimes`).
pub(crate) fn note_ctime(cx: &Ctx, path: &str, meta: &FileMeta) {
    if meta.ctime == 0 || cx.with(|s| s.index.ctimes.get(path) == Some(&meta.ctime)) {
        return;
    }
    cx.with_mut(|s| {
        s.index.ctimes.insert(path.to_owned(), meta.ctime);
    });
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
            // Исключённое внутри исчезнувшей папки — не удаление (страховка).
            if s.is_excluded(&k) {
                continue;
            }
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

/// Страховка от массового удаления: если полный обход не нашёл столько файлов,
/// это скорее сбой (нет прав на папку, не подключён диск, ошибка ФС), чем работа
/// пользователя — удаления не отправляются, синк останавливается.
const MASS_DELETE_MIN: usize = 50;
/// ...и не меньше этой доли (1/4) файлов, которые здесь были.
const MASS_DELETE_SHARE: usize = 4;
/// Листинг без единого файла при стольких исчезнувших — тоже сбой (пустая точка
/// монтирования).
const EMPTY_LISTING_MIN: usize = 10;

/// Исчезло `gone` файлов из `total`, в листинге осталось `present`: похоже на сбой.
pub(crate) fn mass_delete(gone: usize, total: usize, present: usize) -> bool {
    (gone >= MASS_DELETE_MIN && gone * MASS_DELETE_SHARE >= total)
        || (present == 0 && gone >= EMPTY_LISTING_MIN)
}

/// Останавливает синк перед массовым удалением, если пользователь его не
/// подтвердил (`UiCommand::AllowDeletes`). Следующий цикл — снова полный обход:
/// вернулись файлы — синк идёт дальше как обычно.
fn hold_mass_delete(
    cx: &Ctx,
    present: &BTreeMap<String, FileMeta>,
    gone: &[String],
) -> SyncResult<()> {
    let allowed = cx.with_mut(|s| std::mem::take(&mut s.allow_deletes));
    let (files, total) = cx.with(|s| {
        let file = |p: &String| s.index.files.get(p).is_some_and(|f| !f.folder);
        let files = gone.iter().filter(|p| file(p)).count();
        let total = s
            .index
            .files
            .values()
            .filter(|f| f.local.is_some() && !f.folder)
            .count();
        (files, total)
    });
    let left = present.values().filter(|m| !m.dir).count();
    if !mass_delete(files, total, left) {
        return Ok(());
    }
    if allowed {
        cx.log(
            LogLevel::Info,
            format!("удаление {files} из {total} файлов подтверждено"),
        );
        return Ok(());
    }
    cx.with_mut(|s| s.need_full_scan = true);
    cx.log(
        LogLevel::Warn,
        format!(
            "полный обход не нашёл {files} из {total} файлов: удаления не отправлены, синк остановлен"
        ),
    );
    cx.notify(Notice::MassDelete {
        count: u32::try_from(files).unwrap_or(u32::MAX),
        total: u32::try_from(total).unwrap_or(u32::MAX),
    });
    Err(SyncError::Blocked("mass_delete".into()))
}

/// Символические ссылки из листинга полного обхода: они и всё под ними не
/// синхронизируются (ссылка может вести за пределы vault'а), записи индекса под
/// ними забываются без удаления на сервере. О новых — уведомление.
/// Ссылка исчезла (на её месте обычная папка или ничего) — журнал перечитывается
/// с начала, чтобы скачать пропущенное под ней (как при сужении исключений).
/// Возвращает листинг без ссылок.
fn take_links(cx: &Ctx, listing: Vec<FileMeta>) -> Vec<FileMeta> {
    let (links, files): (Vec<FileMeta>, Vec<FileMeta>) = listing.into_iter().partition(|m| m.link);
    let found: BTreeSet<String> = links
        .iter()
        .filter_map(|m| VaultPath::normalize(&m.path).ok())
        .map(|p| p.as_str().to_owned())
        .collect();
    let (fresh, replay, forgot) = cx.with_mut(|s| {
        let old = std::mem::replace(&mut s.index.links, found);
        let freed = old.iter().any(|l| !s.under_link(l));
        let replay = freed && s.index.initial_done && s.index.last_seq > 0;
        if replay {
            s.index.last_seq = 0;
        }
        // О ссылке — один раз, когда её нашёл полный обход (найденные `stat` ещё
        // не объявлены). Ссылки в исключённом (папка плагина в разработке и т.п.) —
        // не новость.
        let hits = std::mem::take(&mut s.link_hits);
        let fresh: Vec<String> = s
            .index
            .links
            .iter()
            .filter(|l| (!old.contains(*l) || hits.contains(*l)) && !s.excludes.is_excluded(l))
            .cloned()
            .collect();
        (fresh, replay, s.forget_excluded())
    });
    if !forgot.is_empty() {
        cx.log(
            LogLevel::Info,
            format!("пути под символическими ссылками забыты (на сервере ничего не меняется): {forgot:?}"),
        );
    }
    if replay {
        cx.log(
            LogLevel::Info,
            "символическая ссылка исчезла: журнал перечитывается с начала".to_owned(),
        );
    }
    if !fresh.is_empty() {
        cx.log(
            LogLevel::Warn,
            format!("символические ссылки не синхронизируются: {fresh:?}"),
        );
        cx.notify(Notice::LinksSkipped { paths: fresh });
    }
    files
}

/// Ошибка ввода-вывода с одним файлом (например, временное имя длиннее, чем
/// позволяет ФС): файл пропускается до следующего обхода и удалённым не считается, а
/// не обрывает весь цикл. Прочие ошибки (сеть, остановка) пробрасываются.
fn skip_stuck(cx: &Ctx, raw: &str, e: SyncError) -> SyncResult<Option<String>> {
    let SyncError::Io(msg) = e else {
        return Err(e);
    };
    cx.log(
        LogLevel::Warn,
        format!("файл пропущен до следующего обхода: {raw}: {msg}"),
    );
    let key =
        VaultPath::normalize(raw).map_or_else(|_| raw.to_owned(), |vp| vp.as_str().to_owned());
    Ok(Some(key))
}

/// Сканирование: полный обход или пути из событий.
pub(crate) async fn scan(cx: &Ctx) -> SyncResult<()> {
    let ev = cx.with(|s| s.renames.clone());
    if !ev.is_empty() {
        cx.log(LogLevel::Debug, format!("rename events {ev:?}"));
    }
    apply_rename_events(cx).await?;
    let full = cx.with(|s| {
        s.need_full_scan
            || s.last_full_scan == 0
            || s.now - s.last_full_scan >= i64::try_from(s.cfg.full_scan_ms).unwrap_or(i64::MAX)
    });
    let mut delta = ScanDelta::default();
    if full {
        // Страховка: исключённый путь в индексе не должен «исчезнуть» и уйти на
        // сервер удалением (обычно его уже забыл Configure).
        let forgot = cx.with_mut(|s| s.forget_excluded());
        if !forgot.is_empty() {
            cx.log(
                LogLevel::Info,
                format!("забыты исключённые пути: {forgot:?}"),
            );
        }
        let listing = take_links(cx, cx.list().await?);
        let verify = cx.with_mut(|s| {
            s.need_full_scan = false;
            s.last_full_scan = s.now;
            s.dirty.clear();
            std::mem::take(&mut s.verify_content)
        });
        if verify {
            cx.log(
                LogLevel::Info,
                "проверка содержимого: все файлы хэшируются заново".to_owned(),
            );
        }
        let mut present: BTreeMap<String, FileMeta> = BTreeMap::new();
        // Файлы, с которыми не вышло (переименование в NFC, доведение брошенного
        // временного файла): они на диске, удалёнными не считаются.
        let mut stuck: BTreeSet<String> = BTreeSet::new();
        for mut m in listing {
            let raw = m.path.clone();
            if m.unreadable {
                // В листинге есть, а метаданных нет: не наблюдается, но и удалённым
                // не считается (иначе удаление ушло бы на все устройства).
                stuck.extend(skip_stuck(
                    cx,
                    &raw,
                    SyncError::Io("метаданные файла недоступны".into()),
                )?);
                continue;
            }
            let p = async {
                if m.path.ends_with(super::ctx::TEMP_SUFFIX)
                    && !cx.with(|s| s.excludes.is_excluded(&m.path))
                {
                    match recover_temp(cx, &m.path).await? {
                        Some(p) => m.path = p,
                        None => return Ok(None),
                    }
                }
                canonical(cx, &m.path, m.dir).await
            }
            .await;
            match p {
                Ok(Some(p)) => {
                    present.insert(p, m);
                }
                Ok(None) => {}
                Err(e) => stuck.extend(skip_stuck(cx, &raw, e)?),
            }
        }
        for (p, m) in &present {
            observe(cx, p, m, verify, &mut delta).await?;
        }
        cx.with_mut(|s| {
            s.index.ctimes.retain(|k, _| present.contains_key(k));
            s.racy.retain(|k| present.contains_key(k));
        });
        let tracked: Vec<String> = cx.with(|s| {
            s.index
                .files
                .iter()
                .filter(|(_, f)| f.local.is_some())
                .map(|(k, _)| k.clone())
                .collect()
        });
        let gone: Vec<String> = tracked
            .into_iter()
            .filter(|p| {
                !present.contains_key(p) && !stuck.contains(p) && !cx.with(|s| s.is_excluded(p))
            })
            .collect();
        hold_mass_delete(cx, &present, &gone)?;
        for p in gone {
            mark_gone(cx, &p, &mut delta);
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
                    match canonical(cx, &actual, m.dir).await {
                        Ok(Some(p)) => observe(cx, &p, &m, false, &mut delta).await?,
                        Ok(None) => {}
                        Err(e) => {
                            skip_stuck(cx, &actual, e)?;
                        }
                    }
                }
                None => {
                    if let Ok(vp) = VaultPath::normalize(&raw)
                        && !cx.with(|s| s.is_excluded(vp.as_str()))
                    {
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

#[cfg(test)]
mod tests {
    use super::strip_tag;

    #[test]
    fn temp_tags() {
        assert_eq!(strip_tag("a.md.nfc", ".nfc"), Some("a.md"));
        assert_eq!(strip_tag("a.md.nfc-3", ".nfc"), Some("a.md"));
        assert_eq!(strip_tag("a.md.case-12", ".case"), Some("a.md"));
        assert_eq!(strip_tag("a.md.nfc-", ".nfc"), None);
        assert_eq!(strip_tag("a.md.nfc-x", ".nfc"), None);
        assert_eq!(strip_tag("a-2.md", ".nfc"), None);
    }
}
