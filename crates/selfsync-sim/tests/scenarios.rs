//! Обязательные сценарии раздела 13.2: каждый — точная последовательность событий
//! без случайных сбоев, затем успокоение и проверка инвариантов (ни одна правка не
//! потеряна, все клиенты совпадают с сервером) и ожидаемого исхода.

use selfsync_core::engine::{Event, Notice};
use selfsync_core::path::NameRules;
use selfsync_sim::{PASSWORD, SimConfig, World, tokens_in};

fn quiet(clients: usize) -> SimConfig {
    let mut c = SimConfig::new(1);
    c.clients = clients;
    c.steps = 0;
    c.fault_rate = 0.0;
    c.kill_rate = 0.0;
    c.skip_event_rate = 0.0;
    c.skew_ms = vec![0; clients];
    c.case_insensitive = vec![false; clients];
    c
}

/// Успокоение и оба инварианта.
fn finish(w: &mut World) {
    if let Err(e) = w.settle() {
        panic!("{e}\n{}", w.dump_trace());
    }
    w.check_no_loss().unwrap();
    w.check_converged().unwrap();
}

/// Заметка из пяти строк; возвращает токен первой строки.
fn base_note(w: &mut World, c: usize, path: &str) -> String {
    let t = w.ledger.fresh(c);
    let text = format!("one {t}\ntwo\nthree\nfour\nfive\n");
    w.user_write(c, path, text.into_bytes());
    t
}

/// Заменить строку `line` (с нуля) новой строкой с токеном.
fn edit_line(w: &mut World, c: usize, path: &str, line: usize) -> String {
    let old = String::from_utf8(w.clients[c].fs.read_file(path).cloned().unwrap()).unwrap();
    let t = w.ledger.fresh(c);
    let mut lines: Vec<String> = old.lines().map(str::to_owned).collect();
    lines[line] = format!("{} {t}", lines[line].split(' ').next().unwrap_or("x"));
    w.user_write(c, path, format!("{}\n", lines.join("\n")).into_bytes());
    t
}

fn server_text(w: &World, path: &str) -> String {
    String::from_utf8_lossy(&w.server_files()[path]).into_owned()
}

fn server_tokens(w: &World) -> Vec<String> {
    w.server_files()
        .values()
        .flat_map(|d| tokens_in(d))
        .collect()
}

fn conflict_copies(w: &World) -> Vec<String> {
    w.server_files()
        .into_keys()
        .filter(|p| p.contains(" ("))
        .collect()
}

/// Сколько живых открытых записей осталось на сервере.
fn plaintext_live(w: &World) -> i64 {
    let v = w.state.vault("sim").unwrap();
    let c = v.pool.get().unwrap();
    c.query_row(
        "SELECT COUNT(*) FROM files WHERE substr(path, 1, 1) = x'00' AND deleted = 0",
        rusqlite::params![],
        |r| r.get(0),
    )
    .unwrap()
}

/// Оба клиента знают файл: c0 создал, оба синхронизировались.
fn shared_note(w: &mut World, path: &str) -> String {
    let t = base_note(w, 0, path);
    w.sync(0);
    w.sync(1);
    assert!(
        w.clients[1].fs.read_file(path).is_some(),
        "c1 должен получить {path}"
    );
    t
}

// 1. Одновременная правка одного файла с двух сторон.

#[test]
fn s01_concurrent_edits_merge_cleanly() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let ta = edit_line(&mut w, 0, "a.md", 0);
    let tb = edit_line(&mut w, 1, "a.md", 4);
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    let text = server_text(&w, "a.md");
    assert!(
        text.contains(&ta) && text.contains(&tb),
        "обе правки слиты: {text}"
    );
    assert!(
        conflict_copies(&w).is_empty(),
        "без конфликтной копии: {:?}",
        conflict_copies(&w)
    );
}

#[test]
fn s01_overlapping_edits_keep_both() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let ta = edit_line(&mut w, 0, "a.md", 2);
    let tb = edit_line(&mut w, 1, "a.md", 2);
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    assert!(
        server_text(&w, "a.md").contains(&ta),
        "серверная версия на месте"
    );
    let copies = conflict_copies(&w);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert!(copies[0].contains("conflict"), "{copies:?}");
    assert!(
        server_text(&w, &copies[0]).contains(&tb),
        "локальная версия в копии"
    );
}

// 2. Удаление на A против правки на B.

#[test]
fn s02_delete_vs_edit_edit_wins() {
    // Удаление ушло на сервер первым.
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    w.user_delete(0, "a.md");
    w.sync(0);
    let tb = edit_line(&mut w, 1, "a.md", 1);
    w.sync(1);
    finish(&mut w);
    assert!(server_text(&w, "a.md").contains(&tb));
    assert!(
        w.clients[0].fs.read_file("a.md").is_some(),
        "файл вернулся на A"
    );

    // Правка ушла первой, удаление — потом.
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let tb = edit_line(&mut w, 1, "a.md", 1);
    w.sync(1);
    w.user_delete(0, "a.md");
    w.sync(0);
    finish(&mut w);
    assert!(server_text(&w, "a.md").contains(&tb));
}

// 3. Переименование на A против правки на B.

#[test]
fn s03_rename_vs_edit_edit_follows() {
    for rename_first in [true, false] {
        let mut w = World::new(quiet(2));
        shared_note(&mut w, "a.md");
        let tb;
        if rename_first {
            assert!(w.user_rename(0, "a.md", "b.md"));
            w.sync(0);
            tb = edit_line(&mut w, 1, "a.md", 3);
            w.sync(1);
        } else {
            tb = edit_line(&mut w, 1, "a.md", 3);
            w.sync(1);
            assert!(w.user_rename(0, "a.md", "b.md"));
            w.sync(0);
        }
        finish(&mut w);
        let files = w.server_files();
        assert!(
            !files.contains_key("a.md"),
            "rename_first={rename_first}: {:?}",
            files.keys()
        );
        assert!(
            server_text(&w, "b.md").contains(&tb),
            "rename_first={rename_first}: правка у нового имени"
        );
    }
}

// 4. Обрыв соединения посреди загрузки и повтор.

#[test]
fn s04_dropped_requests_are_retried_without_duplicates() {
    let mut w = World::new(quiet(2));
    for p in ["a.md", "b.md", "img/p.png"] {
        base_note(&mut w, 0, p);
    }
    // Блоб не дошёл; затем ops применились, а ответ потерялся.
    w.planned_faults.push_back(("PUT /v1/blobs/".into(), false));
    w.planned_faults.push_back(("POST /v1/ops".into(), true));
    for _ in 0..4 {
        w.sync(0);
    }
    assert!(w.planned_faults.is_empty(), "оба сбоя сработали");
    finish(&mut w);
    let files = w.server_files();
    assert_eq!(files.len(), 3, "{:?}", files.keys());
    assert!(conflict_copies(&w).is_empty(), "повтор не плодит копий");
}

// 5. Клиент убит между записью файла и обновлением индекса.

#[test]
fn s05_killed_between_write_and_index_checkpoint() {
    // Скачивание нового файла.
    let mut w = World::new(quiet(2));
    base_note(&mut w, 0, "a.md");
    w.sync(0);
    w.deliver(1, Event::SyncNow);
    let mut wrote = false;
    while let Some(l) = w.pump_one(1) {
        if l.starts_with("Write a.md") || l.starts_with("CommitTemp a.md") {
            wrote = true;
            break;
        }
    }
    assert!(wrote, "c1 дошёл до записи файла");
    w.kill(1);
    finish(&mut w);
    assert!(
        conflict_copies(&w).is_empty(),
        "уже записанный файл распознан как свой"
    );

    // Запись результата слияния.
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let ta = edit_line(&mut w, 0, "a.md", 0);
    w.sync(0);
    let tb = edit_line(&mut w, 1, "a.md", 4);
    w.deliver(1, Event::SyncNow);
    while let Some(l) = w.pump_one(1) {
        if l.starts_with("Write a.md") {
            break;
        }
    }
    w.kill(1);
    finish(&mut w);
    let text = server_text(&w, "a.md");
    assert!(text.contains(&ta) && text.contains(&tb), "{text}");
}

// 6. Переименование только регистра.

#[test]
fn s06_case_only_rename() {
    let mut cfg = quiet(2);
    cfg.case_insensitive = vec![false, true];
    let mut w = World::new(cfg);
    shared_note(&mut w, "note.md");
    // На регистрозависимой ФС — дойти до регистронезависимой.
    assert!(w.user_rename(0, "note.md", "Note.md"));
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    assert_eq!(
        w.server_files().into_keys().collect::<Vec<_>>(),
        vec!["Note.md".to_owned()]
    );
    assert_eq!(w.clients[1].fs.display_name("note.md"), Some("Note.md"));
    // И обратно — с регистронезависимой.
    assert!(w.user_rename(1, "Note.md", "NOTE.md"));
    w.sync(1);
    w.sync(0);
    finish(&mut w);
    assert_eq!(
        w.server_files().into_keys().collect::<Vec<_>>(),
        vec!["NOTE.md".to_owned()]
    );
    assert!(w.clients[0].fs.read_file("NOTE.md").is_some());
}

// 7. Unicode в пути: NFC против NFD.

#[test]
fn s07_nfc_vs_nfd() {
    let mut w = World::new(quiet(2));
    let nfd = "Cafe\u{301}.md";
    let nfc = "Caf\u{e9}.md";
    let t0 = w.ledger.fresh(0);
    w.user_write(0, nfd, format!("nfd {t0}\n").into_bytes());
    let t1 = w.ledger.fresh(1);
    w.user_write(1, nfc, format!("nfc {t1}\n").into_bytes());
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    let files = w.server_files();
    for p in files.keys() {
        assert_eq!(
            selfsync_core::path::VaultPath::normalize(p)
                .unwrap()
                .as_str(),
            p,
            "на сервере только NFC"
        );
    }
    let toks = server_tokens(&w);
    assert!(
        toks.contains(&t0) && toks.contains(&t1),
        "обе версии сохранены: {:?}",
        files.keys()
    );
    assert!(
        w.clients[0]
            .fs
            .snapshot()
            .keys()
            .all(|p| !p.contains('\u{301}')),
        "на диске имя приведено к NFC"
    );
}

// 8. Файл 100 МБ, докачка с середины.

#[test]
fn s08_large_file_resumes_mid_transfer() {
    const SIZE: usize = 100 * 1024 * 1024;
    let mut w = World::new(quiet(2));
    let tok = w.ledger.fresh(0);
    let mut data: Vec<u8> = (0..SIZE)
        .map(|i| ((i as u64).wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    data[0] = 0; // бинарный
    data.push(0); // граница слова перед токеном
    data.extend_from_slice(tok.as_bytes());
    w.user_write(0, "big.bin", data.clone());

    // Загрузка: убить клиент после нескольких частей и продолжить.
    w.deliver(0, Event::SyncNow);
    let mut parts = 0;
    while let Some(l) = w.pump_one(0) {
        if l.starts_with("HTTP PUT /v1/uploads/") {
            parts += 1;
            if parts == 10 {
                break;
            }
        }
    }
    assert_eq!(parts, 10, "загрузка шла частями");
    w.kill(0);
    w.sync(0);
    let sent = w.clients[0].sent_bytes;
    assert!(
        sent < SIZE as u64 * 13 / 10,
        "загрузка продолжена, а не начата заново: отправлено {sent}"
    );

    // Скачивание: то же на другом устройстве.
    w.deliver(1, Event::SyncNow);
    let mut ranges = 0;
    while let Some(l) = w.pump_one(1) {
        if l.starts_with("HTTP GET /v1/blobs/") {
            ranges += 1;
            if ranges == 10 {
                break;
            }
        }
    }
    assert_eq!(ranges, 10, "скачивание шло частями");
    w.kill(1);
    finish(&mut w);
    let recv = w.clients[1].recv_bytes;
    assert!(
        recv < SIZE as u64 * 13 / 10,
        "скачивание продолжено: получено {recv}"
    );
    assert_eq!(w.clients[1].fs.read_file("big.bin"), Some(&data));
}

// 9. Часы клиента спешат на сутки.

#[test]
fn s09_clock_one_day_ahead() {
    let mut cfg = quiet(2);
    cfg.skew_ms = vec![86_400_000, 0];
    let mut w = World::new(cfg);
    shared_note(&mut w, "a.md");
    edit_line(&mut w, 0, "a.md", 2);
    edit_line(&mut w, 1, "a.md", 2);
    w.sync(1);
    w.sync(0);
    let tn = base_note(&mut w, 1, "fresh.md");
    w.sync(1);
    finish(&mut w);
    // Порядок решают ревизии сервера, а не часы: новый файл дошёл, конфликт сохранил
    // обе версии, копия помечена датой спешащего устройства.
    assert!(server_tokens(&w).contains(&tn));
    let copies = conflict_copies(&w);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert!(
        copies[0].contains("2026-10-10") && copies[0].contains("dev0"),
        "{copies:?}"
    );
}

// 10. Два клиента создают один и тот же путь с нуля.

#[test]
fn s10_same_path_created_twice() {
    let mut w = World::new(quiet(2));
    let t0 = w.ledger.fresh(0);
    w.user_write(0, "x.md", format!("from a {t0}\n").into_bytes());
    let t1 = w.ledger.fresh(1);
    w.user_write(1, "x.md", format!("from b {t1}\n").into_bytes());
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    assert!(server_text(&w, "x.md").contains(&t0));
    let copies = conflict_copies(&w);
    assert_eq!(copies, vec!["x (dev1 2026-10-09).md".to_owned()]);
    assert!(server_text(&w, &copies[0]).contains(&t1));

    // Одинаковое содержимое — без копии.
    let mut w = World::new(quiet(2));
    let t = w.ledger.fresh(0);
    let body = format!("same {t}\n").into_bytes();
    w.user_write(0, "y.md", body.clone());
    w.user_write(1, "y.md", body);
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    assert!(conflict_copies(&w).is_empty());
}

// Плюс: шифрование включают посреди активного синка с другого устройства.

#[test]
fn encryption_enabled_during_active_sync() {
    let mut w = World::new(quiet(2));
    for p in ["a.md", "b.md", "notes/c.md"] {
        base_note(&mut w, 0, p);
    }
    w.sync(0);
    w.sync(1);
    w.encrypted_password = Some(PASSWORD.to_owned());
    w.deliver(
        0,
        Event::EnableEncryption {
            password: PASSWORD.into(),
            remember: true,
            recovery: false,
        },
    );
    // Пока c0 перезаливает vault, c1 продолжает править и синхронизироваться.
    let mut tokens = Vec::new();
    for i in 0..40 {
        w.pump_one(0);
        if i % 8 == 0 {
            tokens.push(edit_line(&mut w, 1, "b.md", (i / 8) % 5));
            let t = w.ledger.fresh(1);
            w.user_write(1, &format!("new{i}.md"), format!("n {t}\n").into_bytes());
            tokens.push(t);
            w.sync(1);
        }
    }
    finish(&mut w);
    assert_eq!(plaintext_live(&w), 0, "открытых записей не осталось");
    assert!(w.vault_keys().is_some());
    let on_server = server_tokens(&w);
    for t in tokens.iter().filter(|t| !w.ledger.removed.contains(*t)) {
        assert!(
            on_server.contains(t),
            "правка {t} на сервере в зашифрованном виде"
        );
    }
}

// Плюс: миграция, прерванная на половине.

#[test]
fn migration_interrupted_halfway() {
    let mut w = World::new(quiet(2));
    for i in 0..120 {
        let t = w.ledger.fresh(0);
        w.user_write(
            0,
            &format!("n/{i}.md"),
            format!("note {i} {t}\n").into_bytes(),
        );
    }
    w.sync(0);
    w.sync(1);
    w.encrypted_password = Some(PASSWORD.to_owned());
    // Ключ не запоминается: после перезапуска понадобится пароль.
    w.deliver(
        0,
        Event::EnableEncryption {
            password: PASSWORD.into(),
            remember: false,
            recovery: false,
        },
    );
    w.deliver(0, Event::SyncNow);
    let mut marker = false;
    let mut batches = 0;
    while let Some(l) = w.pump_one(0) {
        if l == "HTTP PUT /v1/vaultkey/migration" {
            marker = true;
        }
        if marker && l == "HTTP POST /v1/ops" {
            batches += 1;
            if batches == 2 {
                break;
            }
        }
    }
    assert_eq!(batches, 2, "миграция дошла до середины");
    assert!(plaintext_live(&w) > 0);
    w.kill(0);
    finish(&mut w);
    assert_eq!(plaintext_live(&w), 0, "миграция доведена после перезапуска");
    assert_eq!(w.server_files().len(), 120);
}

// Исполнитель без атомарной замены убит между удалением X и переименованием
// `X.commit.selfsync-tmp` в X: запись доводится, лишних копий нет.

#[test]
fn interrupted_commit_phase_is_completed() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let ta = edit_line(&mut w, 0, "a.md", 0);
    w.sync(0);
    let fresh = w.server_files()["a.md"].clone();
    let now = w.client_now(1);
    w.clients[1]
        .fs
        .user_write("a.md.commit.selfsync-tmp", fresh, now);
    w.clients[1].fs.user_delete("a.md");
    w.kill(1);
    finish(&mut w);
    assert!(String::from_utf8_lossy(w.clients[1].fs.read_file("a.md").unwrap()).contains(&ta));
    assert!(conflict_copies(&w).is_empty(), "{:?}", conflict_copies(&w));
}

// Имена, которые ФС устройства не может хранить (`?` в имени с Linux на Android).

fn name_notices(w: &World, c: usize) -> (Vec<String>, Vec<String>) {
    let mut unsupported = Vec::new();
    let mut errors = Vec::new();
    for n in &w.clients[c].notices {
        match n {
            Notice::UnsupportedName { path } => unsupported.push(path.clone()),
            Notice::Error { message, .. } => errors.push(message.clone()),
            _ => {}
        }
    }
    (unsupported, errors)
}

#[test]
fn unsupported_name_does_not_block_other_files() {
    let mut cfg = quiet(2);
    cfg.name_rules = vec![NameRules::Any, NameRules::Fat];
    let mut w = World::new(cfg);
    // Заметка с `?` меньше и в очереди pull стоит раньше остальных.
    let tq = w.ledger.fresh(0);
    w.user_write(0, "Мы живём ли?.md", format!("q {tq}\n").into_bytes());
    let ta = base_note(&mut w, 0, "Добро пожаловать.md");
    let tb = base_note(&mut w, 0, "Что?/внутри.md");
    w.sync(0);
    w.sync(1);
    w.sync(1);

    let fs1 = &w.clients[1].fs;
    assert!(
        String::from_utf8_lossy(fs1.read_file("Добро пожаловать.md").unwrap()).contains(&ta),
        "остальные файлы дошли"
    );
    assert!(fs1.read_file("Мы живём ли?.md").is_none());
    assert!(fs1.get("Что?").is_none());
    let (unsupported, errors) = name_notices(&w, 1);
    assert_eq!(
        unsupported,
        vec!["Что?".to_owned(), "Мы живём ли?.md".to_owned()],
        "по одному уведомлению на недопустимое имя, даже после повторных циклов"
    );
    assert!(errors.is_empty(), "{errors:?}");

    // Телефон по-прежнему отправляет своё.
    let tc = base_note(&mut w, 1, "с телефона.md");
    w.sync(1);
    w.sync(0);
    assert!(
        String::from_utf8_lossy(w.clients[0].fs.read_file("с телефона.md").unwrap()).contains(&tc)
    );

    // На ПК имя исправили — файл приходит.
    assert!(w.user_rename(0, "Мы живём ли?.md", "Мы живём ли.md"));
    assert!(w.user_rename(0, "Что?", "Что"));
    w.sync(0);
    w.sync(1);
    let fs1 = &w.clients[1].fs;
    assert!(String::from_utf8_lossy(fs1.read_file("Мы живём ли.md").unwrap()).contains(&tq));
    assert!(String::from_utf8_lossy(fs1.read_file("Что/внутри.md").unwrap()).contains(&tb));
    finish(&mut w);
}

#[test]
fn rename_to_unsupported_name_keeps_local_edits() {
    let mut cfg = quiet(2);
    cfg.name_rules = vec![NameRules::Any, NameRules::Fat];
    let mut w = World::new(cfg);
    shared_note(&mut w, "a.md");
    let t1 = edit_line(&mut w, 1, "a.md", 2);
    assert!(w.user_rename(0, "a.md", "a?.md"));
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    // Правка телефона не потеряна (finish проверил), и файл на сервере есть под
    // именем, доступным телефону.
    let server = w.server_files();
    assert!(
        server
            .iter()
            .any(|(p, d)| !p.contains('?') && String::from_utf8_lossy(d).contains(&t1)),
        "{:?}",
        server.keys().collect::<Vec<_>>()
    );
    assert_eq!(name_notices(&w, 1).0, vec!["a?.md".to_owned()]);
}

#[test]
fn local_write_failure_does_not_block_other_files() {
    // Ядро о запрете не знает (правила `Any`), а ФС пишет с ошибкой: файл
    // откладывается, остальные применяются, ошибка показывается один раз.
    let mut w = World::new(quiet(2));
    w.clients[1].fs.name_rules = NameRules::Fat;
    let tq = w.ledger.fresh(0);
    w.user_write(0, "q?.md", format!("q {tq}\n").into_bytes());
    let ta = base_note(&mut w, 0, "after.md");
    w.sync(0);
    w.sync(1);
    w.sync(1);
    w.sync(1);
    assert!(String::from_utf8_lossy(w.clients[1].fs.read_file("after.md").unwrap()).contains(&ta));
    let (unsupported, errors) = name_notices(&w, 1);
    assert!(unsupported.is_empty());
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("q?.md"), "{errors:?}");

    // Запрет сняли (например, исправили ФС) — отложенная запись применяется.
    w.clients[1].fs.name_rules = NameRules::Any;
    w.sync(1);
    assert!(w.clients[1].fs.read_file("q?.md").is_some());
    finish(&mut w);
}

#[test]
fn non_portable_name_warns_once_on_upload() {
    let mut w = World::new(quiet(1));
    w.user_write(0, "why?.md", b"x\n".to_vec());
    w.user_write(0, "CON.md", b"y\n".to_vec());
    w.sync(0);
    w.user_write(0, "why?.md", b"x2\n".to_vec());
    w.sync(0);
    let warned: Vec<String> = w.clients[0]
        .notices
        .iter()
        .filter_map(|n| match n {
            Notice::NonPortableName { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(warned, vec!["CON.md".to_owned(), "why?.md".to_owned()]);
    finish(&mut w);
}

// Одно и то же переименование на двух устройствах: ревизии нового пути
// начинаются заново, и rev 1 нового пути — не база того, кто переименовал вторым.

#[test]
fn same_rename_on_both_sides_keeps_both_edits() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    // c1 правит и переименовывает, сервер видит обе операции.
    let t1 = edit_line(&mut w, 1, "a.md", 1);
    w.sync(1);
    assert!(w.user_rename(1, "a.md", "b.md"));
    w.sync(1);
    // c0 ещё не видел правку: то же переименование и своя правка.
    assert!(w.user_rename(0, "a.md", "b.md"));
    let t0 = edit_line(&mut w, 0, "b.md", 3);
    w.sync(0);
    finish(&mut w);
    let all: String = w
        .server_files()
        .values()
        .map(|d| String::from_utf8_lossy(d).into_owned())
        .collect();
    assert!(all.contains(&t0), "правка c0 на месте: {all}");
    assert!(all.contains(&t1), "правка c1 не вытеснена: {all}");
}

// Файл с тем же путём лежал локально (ещё не в индексе) и пропал между stat и
// чтением: запись сервера не должна считаться применённой.

#[test]
fn local_file_vanishing_mid_pull_does_not_drop_server_file() {
    let mut w = World::new(quiet(2));
    w.sync(0);
    let t = base_note(&mut w, 1, "n.md");
    w.sync(1);
    w.deliver(0, Event::SyncNow);
    // Скан уже прошёл: файл, появившийся теперь, в индекс не попал.
    while let Some(label) = w.pump_one(0) {
        if label.starts_with("HTTP GET /v1/changes") && label.ends_with("limit=1000") {
            break;
        }
    }
    let now = w.client_now(0);
    w.clients[0]
        .fs
        .user_write("n.md", b"local draft\n".to_vec(), now);
    while let Some(label) = w.pump_one(0) {
        if label == "Stat" {
            break;
        }
    }
    // Пользователь удалил файл, пока ядро сверяло его с серверным.
    w.clients[0].fs.user_delete("n.md");
    w.drain(0);
    w.sync(0);
    let got = w.clients[0].fs.read_file("n.md").cloned();
    assert!(
        got.is_some_and(|d| String::from_utf8_lossy(&d).contains(&t)),
        "серверный файл пришёл"
    );
    finish(&mut w);
}

#[test]
fn rate_limited_client_waits_retry_after() {
    let mut w = World::new(quiet(2));
    let tok = base_note(&mut w, 0, "a.md");
    // Лимитер сервера заблокировал общий ключ (в симуляции у запросов нет адреса).
    let block = |w: &World, until: i64| {
        let pool = w.state.server_pool().unwrap();
        pool.get()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO throttle (key, window_start, failures, blocked_until)
                 VALUES ('auth:*', 0, 30, ?1)",
                [until],
            )
            .unwrap();
    };
    block(&w, selfsync_server::db::now_ms() + 120_000);
    w.user_edit(1, "a.md");
    w.sync(1);
    let st = w.clients[1].status.clone().unwrap();
    assert_eq!(st.reason.as_deref(), Some("rate_limited"));
    let retry_at = st.retry_at;
    let wait = retry_at - w.client_now(1);
    assert!((119_000..=120_000).contains(&wait), "{wait}");
    // ни «синхронизировать сейчас», ни таймеры не шлют запросов до конца ожидания
    let before = w.clients[1].requests;
    w.sync(1);
    w.now += 60_000;
    w.tick(1);
    w.drain(1);
    w.sync(1);
    assert_eq!(w.clients[1].requests, before, "повтор раньше Retry-After");
    assert!(w.clients[1].wake.is_some_and(|at| at >= retry_at));
    // блокировка снята, время вышло — синк проходит сам
    block(&w, 0);
    w.now = retry_at;
    w.tick(1);
    w.drain(1);
    let st = w.clients[1].status.clone().unwrap();
    assert_eq!(st.reason, None, "{st:?}");
    assert_eq!(st.retry_at, 0);
    assert!(w.clients[1].requests > before);
    finish(&mut w);
    assert!(server_text(&w, "a.md").contains(&tok));
}

/// Устройство только для чтения: получает чужие правки, свои не отправляет, а правка
/// поверх пришедшей серверной версии сохраняется копией рядом.
fn read_only_scenario(old_server: bool) {
    let mut w = World::new(quiet(2));
    w.hide_read_only = old_server;
    let shared = shared_note(&mut w, "a.md");
    w.set_read_only(1);
    // чужая правка приходит
    let from_c0 = edit_line(&mut w, 0, "a.md", 1);
    w.sync(0);
    // своя новая заметка остаётся только здесь
    let local_new = base_note(&mut w, 1, "mine.md");
    w.sync(1);
    let text = String::from_utf8(w.clients[1].fs.read_file("a.md").cloned().unwrap()).unwrap();
    assert!(text.contains(&from_c0) && text.contains(&shared), "{text}");
    let st = w.clients[1].status.clone().unwrap();
    assert!(st.read_only, "{st:?}");
    assert_eq!(st.pending, 1, "{st:?}");
    assert!(!w.server_files().contains_key("mine.md"));
    let ro_notices = |w: &World| {
        w.clients[1]
            .notices
            .iter()
            .filter(|n| matches!(n, Notice::ReadOnly { .. }))
            .count()
    };
    assert_eq!(ro_notices(&w), 1);
    // правка поверх серверной: серверная на месте, своя — копией рядом
    let mine = edit_line(&mut w, 1, "a.md", 3);
    let theirs = edit_line(&mut w, 0, "a.md", 3);
    w.sync(0);
    for _ in 0..3 {
        w.now += 20_000;
        w.sync(1);
    }
    let local = w.clients[1].fs.snapshot();
    let a = String::from_utf8_lossy(&local["a.md"]).into_owned();
    assert!(a.contains(&theirs) && !a.contains(&mine), "{a}");
    let copies: Vec<&String> = local.keys().filter(|p| p.starts_with("a (")).collect();
    assert_eq!(copies.len(), 1, "{:?}", local.keys());
    assert!(String::from_utf8_lossy(&local[copies[0]]).contains(&mine));
    // на сервер ничего не ушло, уведомление — одно за сессию
    let server = server_tokens(&w);
    assert!(!server.contains(&mine) && !server.contains(&local_new));
    assert_eq!(ro_notices(&w), 1);
    // запросы на запись: ни одного, а со старым сервером — один отказ за сессию
    assert_eq!(w.clients[1].writes, u64::from(old_server));
    w.kill(1);
    w.sync(1);
    w.now += 20_000;
    w.sync(1);
    assert_eq!(w.clients[1].writes, 2 * u64::from(old_server));
    assert!(w.clients[1].status.as_ref().unwrap().read_only);
    // устройство с записью сходится с сервером, ни одна правка не потеряна
    w.check_no_loss().unwrap();
    let server = w.server_files();
    let c0: std::collections::BTreeMap<_, _> = w.clients[0].fs.snapshot().into_iter().collect();
    assert_eq!(c0, server);
}

#[test]
fn read_only_client_receives_but_does_not_send() {
    read_only_scenario(false);
}

#[test]
fn read_only_client_against_old_server_learns_from_403() {
    read_only_scenario(true);
}

/// Последнее `Synced` клиента с начала отсчёта: (pulled, pushed, paths).
fn synced_since(w: &World, c: usize, from: usize) -> Vec<(u32, u32, Vec<String>)> {
    w.clients[c].notices[from..]
        .iter()
        .filter_map(|n| match n {
            Notice::Synced { pulled, pushed } => {
                let n = |v: &Vec<String>| u32::try_from(v.len()).unwrap();
                let mut paths: Vec<String> = pulled.iter().chain(pushed).cloned().collect();
                paths.sort();
                paths.dedup();
                Some((n(pulled), n(pushed), paths))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn synced_notice_lists_cycle_changes() {
    let mut w = World::new(quiet(2));
    w.sync(0);
    w.sync(1);
    // пустой цикл — без уведомления
    let n0 = w.clients[0].notices.len();
    w.sync(0);
    assert!(synced_since(&w, 0, n0).is_empty());
    base_note(&mut w, 0, "a.md");
    base_note(&mut w, 0, "dir/b.md");
    w.sync(0);
    let s = synced_since(&w, 0, n0);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0].1, 3, "{s:?}"); // папка dir и два файла
    assert_eq!(s[0].0, 0, "{s:?}");
    assert!(s[0].2.contains(&"dir/b.md".to_owned()));
    let n1 = w.clients[1].notices.len();
    w.sync(1);
    let s = synced_since(&w, 1, n1);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!((s[0].1, s[0].2.contains(&"a.md".to_owned())), (0, true));
    // удаление и переименование тоже попадают в список
    w.user_delete(0, "a.md");
    w.user_rename(0, "dir/b.md", "dir/c.md");
    w.sync(0);
    let n1 = w.clients[1].notices.len();
    w.sync(1);
    let s = synced_since(&w, 1, n1);
    let paths: Vec<String> = s.iter().flat_map(|x| x.2.clone()).collect();
    for p in ["a.md", "dir/b.md", "dir/c.md"] {
        assert!(paths.contains(&p.to_owned()), "{p}: {paths:?}");
    }
    // зашифрованный vault: пути открытые
    w.encrypted_password = Some(PASSWORD.to_owned());
    w.deliver(
        0,
        Event::EnableEncryption {
            password: PASSWORD.into(),
            remember: true,
            recovery: false,
        },
    );
    w.drain(0);
    finish(&mut w);
    base_note(&mut w, 0, "secret/d.md");
    w.sync(0);
    let n1 = w.clients[1].notices.len();
    w.sync(1);
    let s = synced_since(&w, 1, n1);
    assert!(
        s.iter().any(|x| x.2.contains(&"secret/d.md".to_owned())),
        "{s:?}"
    );
}

/// Сколько записей в журнале сервера (каждая операция двигает seq).
fn seq(w: &World) -> u64 {
    w.server_seq()
}

#[test]
fn excluding_synced_path_changes_nothing_on_server() {
    let mut w = World::new(quiet(2));
    let a = base_note(&mut w, 0, "notes/a.md");
    let b = base_note(&mut w, 0, "top.md");
    w.sync(0);
    w.sync(1);
    let before = seq(&w);
    // c1 исключает notes/: файлы остаются на месте, на сервер ничего не уходит
    w.set_excludes(1, &["notes/"]);
    for _ in 0..3 {
        w.now += 20_000;
        w.sync(1);
    }
    assert_eq!(seq(&w), before, "исключение не породило операций");
    assert!(server_text(&w, "notes/a.md").contains(&a));
    assert!(
        w.clients[1].fs.read_file("notes/a.md").is_some(),
        "локальный файл цел"
    );
    // пока путь исключён: c0 правит его, c1 удаляет свою копию — сервер не трогается
    let from_c0 = edit_line(&mut w, 0, "notes/a.md", 2);
    w.sync(0);
    w.user_delete(1, "notes/a.md");
    w.sync(1);
    assert!(server_text(&w, "notes/a.md").contains(&from_c0));
    // снова включить: файл приходит с сервера в актуальном виде
    w.set_excludes(1, &[]);
    w.sync(1);
    let text =
        String::from_utf8(w.clients[1].fs.read_file("notes/a.md").cloned().unwrap()).unwrap();
    assert!(text.contains(&from_c0), "{text}");
    assert!(server_text(&w, "top.md").contains(&b));
    finish(&mut w);
    assert!(conflict_copies(&w).is_empty());
}

#[test]
fn including_subdir_pulls_what_arrived_while_excluded() {
    let mut w = World::new(quiet(2));
    base_note(&mut w, 0, "top.md");
    w.sync(0);
    w.sync(1);
    // c1 с самого начала исключает cfg/, кроме cfg/keep.json
    w.set_excludes(1, &["cfg/", "!cfg/keep.json"]);
    w.sync(1);
    let k = w.ledger.fresh(0);
    w.user_write(
        0,
        "cfg/keep.json",
        format!("{{\"k\":\"{k}\"}}\n").into_bytes(),
    );
    let o = w.ledger.fresh(0);
    w.user_write(
        0,
        "cfg/other.json",
        format!("{{\"o\":\"{o}\"}}\n").into_bytes(),
    );
    w.user_write(0, "cfg/sub/deep.json", b"{}\n".to_vec());
    w.sync(0);
    w.sync(1);
    assert!(
        w.clients[1].fs.read_file("cfg/keep.json").is_some(),
        "включённый путь приходит"
    );
    assert!(w.clients[1].fs.read_file("cfg/other.json").is_none());
    // c0 продолжает: новая ревизия other.json, пока у c1 он исключён
    w.user_write(
        0,
        "cfg/other.json",
        format!("{{\"o\":\"{o}\",\"v\":2}}\n").into_bytes(),
    );
    w.sync(0);
    w.sync(1);
    // включить всё: пропущенное подтягивается повтором журнала
    let before = seq(&w);
    w.set_excludes(1, &[]);
    w.sync(1);
    let other = String::from_utf8(
        w.clients[1]
            .fs
            .read_file("cfg/other.json")
            .cloned()
            .unwrap(),
    )
    .unwrap();
    assert!(other.contains("\"v\":2"), "{other}");
    assert!(w.clients[1].fs.read_file("cfg/sub/deep.json").is_some());
    assert_eq!(
        seq(&w),
        before,
        "повтор журнала ничего не записал на сервер"
    );
    finish(&mut w);
    assert!(conflict_copies(&w).is_empty());
}

#[test]
fn replay_over_synced_files_is_a_no_op() {
    let mut w = World::new(quiet(2));
    for p in ["a.md", "dir/b.md", "dir/sub/c.md", "img/p.png"] {
        base_note(&mut w, 0, p);
    }
    w.sync(0);
    w.sync(1);
    edit_line(&mut w, 1, "a.md", 1);
    w.sync(1);
    w.sync(0);
    let before = seq(&w);
    let snap0 = w.clients[0].fs.snapshot();
    // Добавить и убрать исключение — второе сужает правила: повтор с начала журнала.
    w.set_excludes(0, &["zzz/"]);
    w.sync(0);
    w.set_excludes(0, &[]);
    let n0 = w.clients[0].notices.len();
    let trace_from = w.trace.len();
    w.sync(0);
    assert!(
        w.trace
            .iter()
            .skip(trace_from)
            .any(|l| l.contains("changes?since=0&")),
        "повтор журнала с начала"
    );
    assert_eq!(seq(&w), before, "ни одной записи");
    assert_eq!(w.clients[0].fs.snapshot(), snap0, "ни одной копии и правки");
    assert!(w.clients[0].engine.as_ref().unwrap().index().last_seq >= before);
    let notices: Vec<_> = w.clients[0].notices[n0..]
        .iter()
        .filter(|n| !matches!(n, Notice::Synced { .. }))
        .collect();
    assert!(notices.is_empty(), "{notices:?}");
    finish(&mut w);
}

#[test]
fn rules_changed_between_restarts_are_applied() {
    // CLI правит конфиг при остановленной службе: события Configure не будет.
    let mut w = World::new(quiet(2));
    base_note(&mut w, 0, "notes/a.md");
    w.sync(0);
    w.sync(1);
    let before = seq(&w);
    w.clients[1].cfg.excludes = vec![".trash/".into(), "notes/".into()];
    w.kill(1);
    w.sync(1);
    w.user_delete(1, "notes/a.md");
    w.now += 20_000;
    w.sync(1);
    assert_eq!(
        seq(&w),
        before,
        "исключённое после перезапуска не удаляется на сервере"
    );
    edit_line(&mut w, 0, "notes/a.md", 1);
    w.sync(0);
    w.clients[1].cfg.excludes = vec![".trash/".into()];
    w.kill(1);
    w.sync(1);
    assert!(
        w.clients[1].fs.read_file("notes/a.md").is_some(),
        "вернулось после включения"
    );
    finish(&mut w);
}

fn set_no_merge(w: &mut World, c: usize, pats: &[&str]) {
    w.clients[c].cfg.no_merge = pats.iter().map(|s| (*s).to_owned()).collect();
    let config = w.clients[c].cfg.clone();
    w.deliver(c, Event::Configure { config });
}

fn json(w: &World, c: usize, path: &str) -> String {
    String::from_utf8(w.clients[c].fs.read_file(path).cloned().unwrap()).unwrap()
}

#[test]
fn no_merge_files_take_the_later_version_without_copies() {
    let mut w = World::new(quiet(2));
    for c in 0..2 {
        set_no_merge(&mut w, c, &["cfg/**"]);
    }
    let base = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
    w.user_write(0, "cfg/hotkeys.json", base.as_bytes().to_vec());
    w.sync(0);
    w.sync(1);
    // обе правки — в соседних строках, diff3 слил бы их в невалидный JSON
    let t0 = w.ledger.fresh(0);
    w.user_write(
        0,
        "cfg/hotkeys.json",
        format!("{{\n  \"a\": \"{t0}\",\n  \"b\": 2\n}}\n").into_bytes(),
    );
    w.now += 5_000;
    let t1 = w.ledger.fresh(1);
    w.user_write(
        1,
        "cfg/hotkeys.json",
        format!("{{\n  \"a\": 1,\n  \"b\": \"{t1}\"\n}}\n").into_bytes(),
    );
    // позже правившее устройство синхронизируется первым — его версия и остаётся
    w.sync(1);
    w.sync(0);
    w.sync(1);
    assert_eq!(
        json(&w, 0, "cfg/hotkeys.json"),
        json(&w, 1, "cfg/hotkeys.json")
    );
    let text = server_text(&w, "cfg/hotkeys.json");
    assert!(text.contains(&t1) && !text.contains(&t0), "{text}");
    assert!(conflict_copies(&w).is_empty(), "без копий рядом");
    assert!(
        w.clients.iter().all(|c| c
            .notices
            .iter()
            .all(|n| !matches!(n, Notice::Conflict { .. }))),
        "без уведомления о конфликте"
    );
    // проигравшая версия не потеряна: в корзине устройства
    w.check_no_loss().unwrap();
    // своя версия новее серверной — уходит поверх, серверная остаётся в истории
    let t1b = w.ledger.fresh(1);
    w.user_write(
        1,
        "cfg/hotkeys.json",
        format!("{{\"v\":\"{t1b}\"}}\n").into_bytes(),
    );
    w.sync(1);
    let t0b = w.ledger.fresh(0);
    w.now += 5_000;
    w.user_write(
        0,
        "cfg/hotkeys.json",
        format!("{{\"v\":\"{t0b}\"}}\n").into_bytes(),
    );
    w.sync(0);
    assert!(server_text(&w, "cfg/hotkeys.json").contains(&t0b));
    let history = w.server_history_texts();
    assert!(
        history
            .iter()
            .any(|h| String::from_utf8_lossy(h).contains(&t1b)),
        "проигравшая версия — в истории на сервере"
    );
    finish(&mut w);
    assert!(conflict_copies(&w).is_empty());
}

#[test]
fn regular_files_still_merge_next_to_no_merge_ones() {
    let mut w = World::new(quiet(2));
    for c in 0..2 {
        set_no_merge(&mut w, c, &["cfg/**"]);
    }
    shared_note(&mut w, "n.md");
    let a = edit_line(&mut w, 0, "n.md", 0);
    let b = edit_line(&mut w, 1, "n.md", 4);
    w.sync(0);
    w.sync(1);
    let text = server_text(&w, "n.md");
    assert!(
        text.contains(&a) && text.contains(&b),
        "заметки сливаются как раньше: {text}"
    );
    finish(&mut w);
}

fn ui_result(
    w: &mut World,
    c: usize,
    req: u64,
    command: selfsync_core::engine::UiCommand,
) -> selfsync_core::engine::UiResult {
    w.deliver(c, Event::Command { req, command });
    w.drain(c);
    w.clients[c]
        .ui_results
        .iter()
        .rev()
        .find(|(r, _)| *r == req)
        .map(|(_, x)| x.clone())
        .expect("ответ команды")
}

#[test]
fn recovery_phrase_restores_access_with_new_password() {
    use selfsync_core::engine::{SyncState, UiCommand, UiResult};
    const NEW: &str = "new password after recovery";
    let mut w = World::new(quiet(3));
    let t = base_note(&mut w, 0, "a.md");
    for c in 0..3 {
        w.sync(c);
    }
    // A включает шифрование с фразой — фраза приходит один раз
    w.encrypted_password = Some(PASSWORD.to_owned());
    w.deliver(
        0,
        Event::EnableEncryption {
            password: PASSWORD.into(),
            remember: true,
            recovery: true,
        },
    );
    w.drain(0);
    let phrases: Vec<Vec<String>> = w.clients[0]
        .notices
        .iter()
        .filter_map(|n| match n {
            Notice::RecoveryPhrase { words } => Some(words.0.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(phrases.len(), 1);
    let phrase = phrases[0].join(" ");
    assert!(w.clients[0].status.as_ref().unwrap().recovery);
    // B — с запомненным ключом, C — без
    w.sync(1);
    w.deliver(
        2,
        Event::Password {
            password: PASSWORD.into(),
            remember: false,
        },
    );
    w.sync(2);
    assert!(w.clients[1].remembered_key.is_some());
    assert!(w.clients[2].remembered_key.is_none());
    // фраза не попала ни в индекс, ни в журнал
    for c in 0..3 {
        let idx = w.clients[c].saved_index.clone().unwrap_or_default();
        assert!(!String::from_utf8_lossy(&idx).contains(&phrases[0][0..3].join(" ")));
    }
    assert!(
        !w.dump_trace().contains(&phrases[0][0..3].join(" ")),
        "фраза в журнале"
    );
    // A забыл пароль: вход по фразе с новым паролем
    w.kill(0);
    w.clients[0].remembered_key = None;
    w.start_client(0);
    w.drain(0);
    let wrong = phrase.replace(
        &phrases[0][0],
        if phrases[0][0] == "zoo" {
            "abandon"
        } else {
            "zoo"
        },
    );
    let r = ui_result(
        &mut w,
        0,
        1,
        UiCommand::Recover {
            phrase: wrong,
            new_password: NEW.into(),
            remember: true,
        },
    );
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "bad_phrase" || code == "wrong_phrase"),
        "{r:?}"
    );
    let r = ui_result(
        &mut w,
        0,
        2,
        UiCommand::Recover {
            phrase: phrase.clone(),
            new_password: NEW.into(),
            remember: true,
        },
    );
    assert_eq!(r, UiResult::Ok);
    w.encrypted_password = Some(NEW.to_owned());
    let t2 = edit_line(&mut w, 0, "a.md", 2);
    w.sync(0);
    // B с запомненным ключом продолжает без пароля
    w.kill(1);
    let n1 = w.clients[1].notices.len();
    w.sync(1);
    assert!(
        !w.clients[1].notices[n1..]
            .iter()
            .any(|n| matches!(n, Notice::NeedPassword)),
        "B не просит пароль"
    );
    let text = String::from_utf8(w.clients[1].fs.read_file("a.md").cloned().unwrap()).unwrap();
    assert!(text.contains(&t2) && text.contains(&t), "{text}");
    // C без запомненного ключа: старый пароль не подходит, новый — да
    w.kill(2);
    w.deliver(2, Event::SyncNow);
    w.drain(2);
    assert_eq!(
        w.clients[2].status.as_ref().unwrap().state,
        SyncState::NeedPassword
    );
    w.deliver(
        2,
        Event::Password {
            password: PASSWORD.into(),
            remember: true,
        },
    );
    w.drain(2);
    assert!(
        w.clients[2]
            .notices
            .iter()
            .any(|n| matches!(n, Notice::WrongPassword))
    );
    w.deliver(
        2,
        Event::Password {
            password: NEW.into(),
            remember: true,
        },
    );
    w.sync(2);
    let text = String::from_utf8(w.clients[2].fs.read_file("a.md").cloned().unwrap()).unwrap();
    assert!(text.contains(&t2), "{text}");
    // фраза по-прежнему действует; «создать новую» её отменяет, «удалить» убирает
    let r = ui_result(
        &mut w,
        0,
        3,
        UiCommand::CreateRecovery {
            password: NEW.into(),
        },
    );
    let UiResult::RecoveryPhrase { words } = r else {
        panic!("{r:?}")
    };
    assert_ne!(words.0.join(" "), phrase);
    let r = ui_result(
        &mut w,
        0,
        4,
        UiCommand::Recover {
            phrase: phrase.clone(),
            new_password: "x".into(),
            remember: true,
        },
    );
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "wrong_phrase"),
        "{r:?}"
    );
    let r = ui_result(
        &mut w,
        0,
        5,
        UiCommand::RemoveRecovery {
            password: NEW.into(),
        },
    );
    assert_eq!(r, UiResult::Ok);
    w.sync(0);
    assert!(!w.clients[0].status.as_ref().unwrap().recovery);
    finish(&mut w);
}

/// Сервер 0.1.x не знает устройств только для чтения: ссылку «только чтение» на
/// нём клиент не создаёт (сервер молча выдал бы полный доступ), обычную — создаёт.
#[test]
fn read_only_join_needs_new_server() {
    use selfsync_core::engine::{UiCommand, UiResult};
    use selfsync_proto::{MINOR_READ_ONLY, PROTO_MINOR};
    let join = |w: &mut World, req: u64, read_only: bool| {
        ui_result(
            w,
            0,
            req,
            UiCommand::CreateJoin {
                name: "tablet".into(),
                read_only,
            },
        )
    };
    let mut w = World::new(quiet(1));
    w.hide_proto_header = true;
    // версия ещё не известна (ни одного ответа) — клиент спросит health
    let r = join(&mut w, 1, true);
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "server_outdated"),
        "{r:?}"
    );
    w.sync(0);
    assert_eq!(w.clients[0].status.as_ref().unwrap().server_minor, Some(0));
    let r = join(&mut w, 2, true);
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "server_outdated"),
        "{r:?}"
    );
    assert!(matches!(join(&mut w, 3, false), UiResult::Join { .. }));
    // сервер обновили: версия приходит в первом же ответе
    w.hide_proto_header = false;
    w.now += 20_000;
    w.sync(0);
    let minor = w.clients[0].status.as_ref().unwrap().server_minor;
    assert_eq!(minor, Some(PROTO_MINOR));
    assert!(minor.unwrap() >= MINOR_READ_ONLY);
    assert!(matches!(join(&mut w, 4, true), UiResult::Join { .. }));
}

/// Файл примерно `size` байт с токеном (для лимита скачивания).
fn sized_file(w: &mut World, c: usize, path: &str, size: usize) -> String {
    let t = w.ledger.fresh(c);
    let mut text = format!("big {t}\n");
    text.push_str(&"x".repeat(size));
    text.push('\n');
    w.user_write(c, path, text.into_bytes());
    t
}

/// Лимит скачивания: большое не скачивается и не считается удалённым, обновления
/// и удаление на сервере отражаются в списке пропущенных, «Скачать» и снятие
/// лимита докачивают.
#[test]
fn download_limit_skips_large_files() {
    use selfsync_core::engine::{SkippedView, UiCommand, UiResult};
    let mut w = World::new(quiet(2));
    w.sync(0);
    w.sync(1);
    w.set_download_limit(1, 200);
    let note = base_note(&mut w, 0, "note.md");
    let big = sized_file(&mut w, 0, "img/big.png", 500);
    sized_file(&mut w, 0, "img/other.png", 600);
    w.sync(0);
    w.sync(1);
    let local = w.clients[1].fs.snapshot();
    assert!(local.contains_key("note.md") && !local.contains_key("img/big.png"));
    let skipped = |w: &mut World, req| match ui_result(w, 1, req, UiCommand::Skipped) {
        UiResult::Skipped { files } => files
            .into_iter()
            .map(|SkippedView { path, .. }| path)
            .collect::<Vec<_>>(),
        r => panic!("{r:?}"),
    };
    assert_eq!(skipped(&mut w, 1), ["img/big.png", "img/other.png"]);
    let notices = |w: &World| {
        w.clients[1]
            .notices
            .iter()
            .filter(|n| matches!(n, Notice::Skipped { .. }))
            .count()
    };
    assert_eq!(notices(&w), 1);
    // своё по-прежнему уходит; пропущенное на сервере цело
    let mine = sized_file(&mut w, 1, "img/mine.png", 900);
    w.sync(1);
    let server = server_tokens(&w);
    assert!(server.contains(&mine) && server.contains(&big) && server.contains(&note));
    // удаление на сервере убирает из списка
    w.user_delete(0, "img/other.png");
    w.sync(0);
    w.now += 20_000;
    w.sync(1);
    assert_eq!(skipped(&mut w, 2), ["img/big.png"]);
    assert_eq!(notices(&w), 1, "уведомление — раз за сессию");
    // скачать один файл несмотря на лимит; дальше он обычный
    assert_eq!(
        ui_result(
            &mut w,
            1,
            3,
            UiCommand::Download {
                path: "img/big.png".into()
            }
        ),
        UiResult::Ok
    );
    w.sync(1);
    assert!(w.clients[1].fs.read_file("img/big.png").is_some());
    assert!(skipped(&mut w, 4).is_empty());
    sized_file(&mut w, 0, "img/big.png", 700);
    w.sync(0);
    w.now += 20_000;
    w.sync(1);
    assert_eq!(
        w.clients[1].fs.read_file("img/big.png"),
        w.clients[0].fs.read_file("img/big.png"),
        "уже скачанный файл обновляется и сверх лимита"
    );
    // новый большой — пропущен; лимит сняли — докачан повтором журнала
    sized_file(&mut w, 0, "img/late.png", 800);
    w.sync(0);
    w.now += 20_000;
    w.sync(1);
    assert!(w.clients[1].fs.read_file("img/late.png").is_none());
    w.set_download_limit(1, 0);
    w.now += 20_000;
    w.sync(1);
    w.sync(1);
    assert!(w.clients[1].fs.read_file("img/late.png").is_some());
    assert!(skipped(&mut w, 5).is_empty());
    finish(&mut w);
    w.check_no_loss().unwrap();
}

/// «Освободить место»: удаляются только исключённые файлы, совпадавшие с сервером;
/// новый, не отправленный файл остаётся. На сервере и других устройствах всё цело.
#[test]
fn remove_local_frees_only_synced_copies() {
    use selfsync_core::engine::{UiCommand, UiResult};
    let mut w = World::new(quiet(2));
    let synced = shared_note(&mut w, "att/a.md");
    shared_note(&mut w, "att/sub/b.md");
    // не исключено — отказ
    let r = ui_result(&mut w, 1, 1, UiCommand::RemoveLocal { path: "att".into() });
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "not_excluded"),
        "{r:?}"
    );
    shared_note(&mut w, "att/c.md");
    // правка, которая не дошла до сервера до исключения (отправка сорвалась)
    let unsent = edit_line(&mut w, 1, "att/sub/b.md", 1);
    w.planned_faults.push_back(("POST /v1/ops".into(), false));
    w.sync(1);
    assert!(!server_tokens(&w).contains(&unsent));
    w.set_excludes(1, &["att/"]);
    w.sync(1);
    // правка и новый файл уже после исключения: на сервер они не уйдут
    let edited = edit_line(&mut w, 1, "att/c.md", 2);
    let fresh = base_note(&mut w, 1, "att/new.md");
    w.sync(1);
    let r = ui_result(&mut w, 1, 2, UiCommand::RemoveLocal { path: "att".into() });
    assert_eq!(
        r,
        UiResult::Removed {
            removed: 1,
            kept: 3
        }
    );
    let local = w.clients[1].fs.snapshot();
    assert!(!local.contains_key("att/a.md"));
    let kept: String = ["att/sub/b.md", "att/c.md", "att/new.md"]
        .iter()
        .map(|p| String::from_utf8_lossy(&local[*p]).into_owned())
        .collect();
    assert!(kept.contains(&unsent) && kept.contains(&edited) && kept.contains(&fresh));
    // на сервере и у другого устройства удаления нет
    w.sync(1);
    w.now += 20_000;
    w.sync(0);
    assert!(server_tokens(&w).contains(&synced));
    assert!(w.clients[0].fs.read_file("att/a.md").is_some());
    // вернули папку — снова скачана
    w.set_excludes(1, &[]);
    w.now += 20_000;
    w.sync(1);
    w.sync(1);
    assert!(w.clients[1].fs.read_file("att/a.md").is_some());
    finish(&mut w);
    w.check_no_loss().unwrap();
}

/// Сколько живых записей на сервере не в этом поколении ключа.
fn stale_epoch_live(w: &World, epoch: u32) -> i64 {
    let v = w.state.vault("sim").unwrap();
    let c = v.pool.get().unwrap();
    let prefix = selfsync_core::path::mode_prefix(true, epoch);
    c.query_row(
        "SELECT COUNT(*) FROM files WHERE substr(path, 1, length(?1)) != ?1 AND deleted = 0",
        rusqlite::params![prefix],
        |r| r.get(0),
    )
    .unwrap()
}

fn record_of(w: &World) -> Vec<u8> {
    let v = w.state.vault("sim").unwrap();
    let c = v.pool.get().unwrap();
    selfsync_server::db::vault::meta_blob(&c, "vault_key")
        .unwrap()
        .unwrap()
}

fn encrypt_all(w: &mut World) {
    w.encrypted_password = Some(PASSWORD.to_owned());
    w.deliver(
        0,
        Event::EnableEncryption {
            password: PASSWORD.into(),
            remember: true,
            recovery: false,
        },
    );
    finish(w);
}

fn rekey(w: &mut World, c: usize, req: u64) {
    use selfsync_core::engine::{UiCommand, UiResult};
    let r = ui_result(
        w,
        c,
        req,
        UiCommand::Rekey {
            password: PASSWORD.into(),
            remember: true,
        },
    );
    assert_eq!(r, UiResult::Ok);
}

/// Смена ключа посреди правок на другом устройстве: после неё на сервере только
/// новое поколение, прежнего ключа в записи нет, ничего не потеряно; вторая смена —
/// поколение 3.
#[test]
fn rekey_during_active_sync() {
    use selfsync_core::crypto;
    let mut w = World::new(quiet(3));
    for p in ["a.md", "b.md", "notes/c.md"] {
        base_note(&mut w, 0, p);
    }
    sized_file(&mut w, 0, "img/big.png", 9 * 1024 * 1024);
    w.sync(0);
    encrypt_all(&mut w);
    let old_record = record_of(&w);
    let old_master = crypto::open_master_key(&old_record, PASSWORD).unwrap();
    rekey(&mut w, 0, 1);
    let mut tokens = Vec::new();
    for i in 0..60 {
        w.pump_one(0);
        if i % 10 == 0 {
            tokens.push(edit_line(&mut w, 1, "b.md", (i / 10) % 5));
            let t = w.ledger.fresh(1);
            w.user_write(1, &format!("new{i}.md"), format!("n {t}\n").into_bytes());
            tokens.push(t);
            w.sync(1);
        }
    }
    finish(&mut w);
    let rec = record_of(&w);
    assert_eq!(crypto::record_epoch(&rec).unwrap(), 2);
    assert!(!crypto::has_prev(&rec), "прежний ключ убран из записи");
    assert!(!crypto::verify_master(&rec, &old_master).unwrap());
    assert_eq!(
        stale_epoch_live(&w, 2),
        0,
        "на сервере только новое поколение"
    );
    let on_server = server_tokens(&w);
    for t in tokens.iter().filter(|t| !w.ledger.removed.contains(*t)) {
        assert!(
            on_server.contains(t),
            "правка {t} на сервере после смены ключа"
        );
    }
    // ни одного блоба, который открывает прежний ключ
    let old_keys = old_master.derive();
    let v = w.state.vault("sim").unwrap();
    for (h, _, _) in v.blobs.list().unwrap() {
        let data = std::fs::read(v.blobs.path_of(&h)).unwrap();
        assert!(
            selfsync_core::blob::decode(&data, Some(&old_keys)).is_err(),
            "блоб {} открывается прежним ключом",
            h.to_hex()
        );
    }
    let done = w.clients[0]
        .notices
        .iter()
        .filter(|n| matches!(n, Notice::RekeyDone))
        .count();
    assert_eq!(done, 1);
    w.check_no_loss().unwrap();
    // ещё раз — поколение 3
    rekey(&mut w, 1, 2);
    finish(&mut w);
    assert_eq!(crypto::record_epoch(&record_of(&w)).unwrap(), 3);
    assert_eq!(stale_epoch_live(&w, 3), 0);
    w.check_no_loss().unwrap();
}

/// Устройство, начавшее смену ключа, убито в любой момент перешифровки — после
/// перезапуска оно её доводит.
#[test]
fn rekey_survives_kills() {
    for kill_at in [1usize, 3, 7, 15, 30] {
        let mut w = World::new(quiet(2));
        for p in ["a.md", "b.md", "c.md", "d.md"] {
            base_note(&mut w, 0, p);
        }
        w.sync(0);
        encrypt_all(&mut w);
        rekey(&mut w, 0, 1);
        for _ in 0..kill_at {
            w.pump_one(0);
        }
        w.kill(0);
        finish(&mut w);
        assert_eq!(
            selfsync_core::crypto::record_epoch(&record_of(&w)).unwrap(),
            2,
            "kill_at {kill_at}"
        );
        assert!(!selfsync_core::crypto::has_prev(&record_of(&w)));
        assert_eq!(stale_epoch_live(&w, 2), 0, "kill_at {kill_at}");
        w.check_no_loss().unwrap();
    }
}

/// Смену ключа бросили сразу после записи ключа (маркер не дошёл), а устройство
/// пропало: другое устройство с паролем подхватывает и доводит её. Вернувшееся —
/// просто переходит на новое поколение.
#[test]
fn abandoned_rekey_is_taken_over() {
    let mut w = World::new(quiet(2));
    for p in ["a.md", "notes/b.md"] {
        base_note(&mut w, 0, p);
    }
    w.sync(0);
    encrypt_all(&mut w);
    let mine = edit_line(&mut w, 1, "a.md", 2);
    w.planned_faults
        .push_back(("PUT /v1/vaultkey/migration".into(), false));
    rekey(&mut w, 0, 1);
    assert!(w.planned_faults.is_empty(), "сбой сработал");
    assert!(selfsync_core::crypto::has_prev(&record_of(&w)));
    // c0 «пропал»; c1 спрашивает пароль, подхватывает, доводит
    for _ in 0..4 {
        w.now += 20_000;
        w.sync(1);
    }
    assert!(!selfsync_core::crypto::has_prev(&record_of(&w)));
    assert_eq!(stale_epoch_live(&w, 2), 0);
    assert!(server_tokens(&w).contains(&mine));
    assert!(
        w.clients[1]
            .notices
            .iter()
            .any(|n| matches!(n, Notice::RekeyDone)),
        "довело второе устройство"
    );
    finish(&mut w);
    assert!(
        !w.clients[0]
            .notices
            .iter()
            .any(|n| matches!(n, Notice::RekeyDone)),
        "вернувшееся не перешифровывает заново"
    );
    w.check_no_loss().unwrap();
}

/// Случайные прогоны действительно меняют ключ (а не только отправляют команду).
#[test]
fn random_runs_exercise_rekey() {
    let mut rekeyed = 0;
    for seed in 0..400 {
        let cfg = SimConfig::random(seed);
        if cfg.rekey_at.is_none() {
            continue;
        }
        let mut w = World::new(cfg);
        w.run();
        w.settle().unwrap();
        w.check_no_loss().unwrap();
        w.check_converged().unwrap();
        if w.server_key_epoch() >= 2 {
            rekeyed += 1;
        }
    }
    assert!(rekeyed >= 5, "смен ключа в прогонах: {rekeyed}");
}

/// Прогоны, на которых ломалась смена ключа: устройство с новым ключом посреди
/// брошенной смены (19748) и устройство с проверенным когда-то ключом, пока ключ
/// дважды сменили (453).
#[test]
fn rekey_regression_seeds() {
    for seed in [453, 19748] {
        selfsync_sim::run_seed(SimConfig::random(seed)).unwrap();
    }
}

// Временное имя двухшагового переименования занято файлом, отличающимся только
// регистром: нормализация и смена регистра берут свободное.

#[test]
fn temp_name_taken_by_user_file() {
    let mut cfg = quiet(2);
    cfg.case_insensitive = vec![true, true];
    let mut w = World::new(cfg);
    let nfc = "Caf\u{e9}.md";
    let t0 = w.ledger.fresh(0);
    w.user_write(
        0,
        &format!("{nfc}.nfc.selfsync-tmP"),
        format!("x {t0}\n").into_bytes(),
    );
    let t1 = w.ledger.fresh(0);
    w.user_write(0, "Cafe\u{301}.md", format!("nfd {t1}\n").into_bytes());
    w.sync(0);
    shared_note(&mut w, "note.md");
    let t2 = w.ledger.fresh(1);
    w.user_write(
        1,
        "Note.md.case.selfsync-tmP",
        format!("y {t2}\n").into_bytes(),
    );
    w.sync(1);
    assert!(w.user_rename(0, "note.md", "Note.md"));
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    let files = w.server_files();
    assert!(
        String::from_utf8_lossy(&files[nfc]).contains(&t1),
        "NFD-файл дошёл под NFC-именем: {:?}",
        files.keys()
    );
    assert_eq!(w.clients[1].fs.display_name("note.md"), Some("Note.md"));
}

// Пока push читал файл, его переименовали и на старом месте создали новый того же
// размера: stat и содержимое — от разных файлов. Наблюдение не должно перейти к
// переименованному файлу и выдать его за неизменённый.

#[test]
fn file_replaced_between_stat_and_read() {
    let mut w = World::new(quiet(2));
    w.sync(0);
    let (ta, tb) = (w.ledger.fresh(0), w.ledger.fresh(0));
    assert_eq!(ta.len(), tb.len());
    w.user_write(0, "x.md", format!("one {ta}\n").into_bytes());
    w.deliver(0, Event::SyncNow);
    while let Some(label) = w.pump_one(0) {
        if label.starts_with("HTTP GET /v1/changes") {
            break;
        }
    }
    assert_eq!(
        w.pump_one(0).as_deref(),
        Some("Stat"),
        "stat перед чтением в push"
    );
    assert!(w.user_rename(0, "x.md", "y.md"));
    w.user_write(0, "x.md", format!("one {tb}\n").into_bytes());
    w.drain(0);
    w.sync(1);
    finish(&mut w);
    let files = w.server_files();
    assert!(String::from_utf8_lossy(&files["y.md"]).contains(&ta));
    assert!(String::from_utf8_lossy(&files["x.md"]).contains(&tb));
}

// Наше переименование отклонено (источник правили), а файл тем временем удалён, и
// событие удаления потерялось. Удаление по старому пути не должно стереть правку,
// которую здесь не видели, а чужой файл на месте назначения — потеряться (номера
// ревизий обоих путей совпадают).

#[test]
fn rejected_rename_then_delete_keeps_unseen_edit() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    assert!(w.user_rename(1, "a.md", "b.md"));
    w.planned_faults.push_back(("POST /v1/ops".into(), false));
    w.sync(1);
    assert!(w.planned_faults.is_empty());
    w.clients[1].fs.user_delete("b.md");
    let te = edit_line(&mut w, 0, "a.md", 2);
    let tb = base_note(&mut w, 0, "b.md");
    w.sync(0);
    let tb2 = edit_line(&mut w, 0, "b.md", 3);
    w.sync(0);
    // Повтор по таймеру — без полного сканирования, удаление не замечено.
    for _ in 0..6 {
        w.now += 10_000;
        w.tick(1);
        w.drain(1);
    }
    finish(&mut w);
    let files = w.server_files();
    assert!(
        files
            .get("a.md")
            .is_some_and(|d| String::from_utf8_lossy(d).contains(&te)),
        "правка c0 не удалена: {:?}",
        files.keys()
    );
    let b = String::from_utf8_lossy(&files["b.md"]).into_owned();
    assert!(b.contains(&tb) && b.contains(&tb2), "{b}");
}

// То же, но источник правят между каждой попыткой переименования: push исчерпывает
// раунды, и pull встречает чужой файл на месте назначения раньше, чем ушло наше
// удаление. Запись нужно перечитать: после удаления по старому пути её здесь не
// останется, а курсор уже ушёл бы дальше.

#[test]
fn rename_rejected_every_round_keeps_destination_file() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    assert!(w.user_rename(1, "a.md", "b.md"));
    w.planned_faults.push_back(("POST /v1/ops".into(), false));
    w.sync(1);
    w.clients[1].fs.user_delete("b.md");
    edit_line(&mut w, 0, "a.md", 1);
    // Шесть раундов push: ревизия a.md дойдёт до 7, у b.md — столько же.
    let tb = base_note(&mut w, 0, "b.md");
    w.sync(0);
    for i in 0..6 {
        edit_line(&mut w, 0, "b.md", 1 + i % 4);
        w.sync(0);
    }
    let mut rounds = 0;
    for _ in 0..6 {
        w.now += 10_000;
        w.tick(1);
        while let Some(label) = w.pump_one(1) {
            if label == "HTTP POST /v1/ops" && rounds < 5 {
                rounds += 1;
                edit_line(&mut w, 0, "a.md", 1 + rounds % 4);
                w.sync(0);
            }
        }
    }
    assert_eq!(rounds, 5, "все раунды push отклонены");
    finish(&mut w);
    let b = w.clients[1]
        .fs
        .read_file("b.md")
        .cloned()
        .unwrap_or_default();
    assert!(
        String::from_utf8_lossy(&b).contains(&tb),
        "b.md c0 пришёл на c1"
    );
}

// Правка победила удаление, но вернуть файл не успели, а пользователь откатил его
// к удалённой версии: файл всё равно возвращается на сервер, а не застревает
// только здесь.

#[test]
fn restored_file_reverted_to_deleted_version_is_pushed() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let before = w.clients[1].fs.read_file("a.md").cloned().unwrap();
    edit_line(&mut w, 1, "a.md", 2);
    assert!(w.clients[0].fs.user_delete("a.md").len() == 1);
    w.deliver(
        0,
        Event::Deleted {
            path: "a.md".into(),
        },
    );
    w.sync(0);
    w.deliver(1, Event::SyncNow);
    while let Some(label) = w.pump_one(1) {
        if label == "HTTP POST /v1/ops" {
            break;
        }
    }
    // Put отклонён (файл удалён), повторный срывается.
    w.planned_faults
        .push_back(("POST /v1/blobs/missing".into(), false));
    w.drain(1);
    assert!(w.planned_faults.is_empty());
    w.user_write(1, "a.md", before.clone());
    w.sync(1);
    w.sync(0);
    finish(&mut w);
    assert_eq!(w.server_files().get("a.md"), Some(&before));
}

#[test]
fn sync_regression_seeds() {
    // Потерянная правка при шифровании (stat/read), не-NFC имя при занятом
    // временном, удаление по чужой ревизии после отклонённого переименования,
    // возвращённый файл, совпавший с базой, открытая запись посреди смены ключа,
    // файл, навсегда ждущий места после снятия квоты, позднее событие переименования
    // после возврата файла, Put по несверенной ревизии после отменённого переименования.
    for seed in [
        25590, 26866, 50362, 57632, 70682, 77055, 81938, 51, 368, 63845, 11870,
    ] {
        selfsync_sim::run_seed(SimConfig::random(seed)).unwrap();
    }
}

// F12. Восстановление блобов с устройств.

/// Блоб, под которым сервер хранит это содержимое (открытый или любым ключом vault'а).
fn blob_of(w: &World, plain: &[u8]) -> selfsync_core::hash::Hash {
    use selfsync_core::{blob, hash::Hash};
    let v = w.state.vault("sim").unwrap();
    let ph = Hash::of(plain);
    let mut cands = vec![Hash::of(&blob::encode(plain, None))];
    for k in w.vault_key_sets() {
        cands.push(Hash::of(&blob::encode_with_hash(plain, ph, Some(&k))));
    }
    cands
        .into_iter()
        .find(|h| v.blobs.exists(h))
        .expect("блоб содержимого на сервере")
}

fn verify(w: &World) -> selfsync_server::verify::VerifyReport {
    selfsync_server::verify::verify_vault(&w.state.config, "sim", Default::default()).unwrap()
}

fn read(w: &World, c: usize, path: &str) -> Vec<u8> {
    w.clients[c].fs.read_file(path).cloned().unwrap()
}

fn blobs_restored_from_devices(encrypted: bool) {
    let mut w = World::new(quiet(2));
    if encrypted {
        encrypt_all(&mut w);
    }
    shared_note(&mut w, "a.md");
    sized_file(&mut w, 0, "big.bin", 9 * 1024 * 1024);
    let old = base_note(&mut w, 0, "h.md");
    w.sync(0);
    let h_old = blob_of(&w, &read(&w, 0, "h.md"));
    edit_line(&mut w, 0, "h.md", 0);
    w.sync(0);
    w.sync(1);
    // Порча на сервере: битый, удалённый большой, удалённый из истории.
    let v = w.state.vault("sim").unwrap();
    let a = blob_of(&w, &read(&w, 1, "a.md"));
    let big = blob_of(&w, &read(&w, 1, "big.bin"));
    std::fs::write(v.blobs.path_of(&a), b"rot").unwrap();
    std::fs::remove_file(v.blobs.path_of(&big)).unwrap();
    std::fs::remove_file(v.blobs.path_of(&h_old)).unwrap();
    let r = verify(&w);
    assert_eq!(r.corrupt, vec![a]);
    assert_eq!(r.missing.len(), 2);
    assert_eq!(r.wanted.len(), 3);
    // Любое устройство с целым файлом заливает его заново.
    w.sync(1);
    let r = verify(&w);
    assert!(r.corrupt.is_empty());
    let left: Vec<_> = r.missing.iter().map(|m| m.hash).collect();
    assert_eq!(left, vec![h_old], "из истории взять неоткуда: {r:?}");
    assert_eq!(
        r.wanted.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![h_old]
    );
    assert!(v.blobs.exists(&a) && v.blobs.exists(&big));
    // Запрос, на который ответить нечем, не разбирается каждый цикл.
    w.sync(0);
    w.trace.clear();
    w.sync(0);
    assert!(
        !w.trace.iter().any(|l| l.contains("/v1/blobs/wanted")),
        "повторный разбор того же запроса"
    );
    finish(&mut w);
    let _ = old;
}

#[test]
fn blobs_restored_from_devices_plain() {
    blobs_restored_from_devices(false);
}

#[test]
fn blobs_restored_from_devices_encrypted() {
    blobs_restored_from_devices(true);
}

fn storage_notices(w: &World, c: usize) -> Vec<(u32, bool)> {
    w.clients[c]
        .notices
        .iter()
        .filter_map(|n| match n {
            Notice::StorageDamaged {
                damaged,
                server_problem,
            } => Some((*damaged, *server_problem)),
            _ => None,
        })
        .collect()
}

fn storage(w: &World, c: usize) -> selfsync_core::engine::StorageHealth {
    w.clients[c].status.as_ref().unwrap().storage
}

/// Итог `verify` доходит до устройств: статус и одно уведомление, пока повреждение
/// не исчезнет; новое повреждение — новое уведомление.
#[test]
fn storage_state_reaches_devices() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    assert_eq!(storage(&w, 0).verified_at, 0, "verify ещё не запускался");
    assert!(verify(&w).clean());
    w.sync(0);
    let s = storage(&w, 0);
    assert!(s.verified_at > 0 && !s.damaged(), "{s:?}");
    assert!(storage_notices(&w, 0).is_empty());

    // Блоб потерян: read-only устройство видит повреждение, но не чинит.
    w.set_read_only(1);
    let v = w.state.vault("sim").unwrap();
    let a = blob_of(&w, &read(&w, 1, "a.md"));
    std::fs::remove_file(v.blobs.path_of(&a)).unwrap();
    verify(&w);
    w.sync(1);
    w.sync(1);
    assert_eq!(storage(&w, 1).damaged, 1);
    assert_eq!(storage_notices(&w, 1), vec![(1, false)], "одно уведомление");

    // Устройство с целой копией чинит; следующий ответ сервера — без повреждений.
    w.sync(0);
    w.sync(0);
    w.sync(1);
    assert!(!storage(&w, 0).damaged() && !storage(&w, 1).damaged());
    assert_eq!(storage_notices(&w, 1).len(), 1);

    // Новое повреждение — снова уведомление; ошибка, которую чинит только админ.
    std::fs::write(v.blobs.path_of(&a), b"rot").unwrap();
    verify(&w);
    {
        let c = v.pool.get().unwrap();
        selfsync_server::db::vault::set_meta_int(&c, "verify_key_problem", 1).unwrap();
    }
    w.sync(1);
    assert_eq!(storage_notices(&w, 1), vec![(1, false), (1, true)]);
    w.sync(0);
    finish(&mut w);
}

#[test]
fn read_only_device_does_not_upload_wanted_blobs() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    w.set_read_only(1);
    let v = w.state.vault("sim").unwrap();
    let a = blob_of(&w, &read(&w, 1, "a.md"));
    std::fs::remove_file(v.blobs.path_of(&a)).unwrap();
    assert_eq!(verify(&w).wanted.len(), 1);
    w.trace.clear();
    w.sync(1);
    assert!(
        !w.trace.iter().any(|l| l.contains("/v1/blobs")),
        "read-only устройство в запросе не участвует: {:?}",
        w.trace
    );
    assert!(!v.blobs.exists(&a));
    assert_eq!(verify(&w).wanted.len(), 1);
    w.sync(0);
    assert!(v.blobs.exists(&a));
    assert!(verify(&w).clean());
}

fn set_hard(w: &World, hard: Option<u64>) {
    use selfsync_server::quota;
    let v = w.state.vault("sim").unwrap();
    let c = v.pool.get().unwrap();
    let mut own = quota::own(&c).unwrap();
    own.hard = hard;
    quota::set_own(&c, &own).unwrap();
}

fn server_used(w: &World) -> u64 {
    let v = w.state.vault("sim").unwrap();
    selfsync_server::quota::usage(&v.pool.get().unwrap()).unwrap()
}

fn quota_of(w: &World, c: usize) -> selfsync_core::engine::QuotaStatus {
    w.clients[c].status.as_ref().unwrap().quota
}

fn quota_full_notices(w: &World, c: usize) -> usize {
    w.clients[c]
        .notices
        .iter()
        .filter(|n| matches!(n, Notice::QuotaFull { .. }))
        .count()
}

fn uploads_in_trace(w: &World) -> bool {
    w.trace
        .iter()
        .any(|l| l.contains("HTTP PUT /v1/blobs") || l.contains("HTTP POST /v1/uploads"))
}

/// Место закончилось: новый файл ждёт, остальное идёт — удаление, переименование,
/// новый маленький файл, правки с другого устройства. Место дали — файл уходит сам.
#[test]
fn quota_full_holds_new_files_until_room() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "keep.md");
    shared_note(&mut w, "gone.md");
    shared_note(&mut w, "old.md");
    set_hard(&w, Some(server_used(&w) + 200));
    sized_file(&mut w, 0, "big.bin", 2000);
    base_note(&mut w, 0, "small.md");
    w.user_delete(0, "gone.md");
    assert!(w.user_rename(0, "old.md", "new.md"));
    w.sync(0);
    let q = quota_of(&w, 0);
    assert_eq!(q.held, 1, "{q:?}");
    assert!(q.hard > 0 && q.used > 0);
    assert_eq!(quota_full_notices(&w, 0), 1);
    w.sync(1);
    let fs1 = &w.clients[1].fs;
    assert!(fs1.read_file("gone.md").is_none());
    assert!(fs1.read_file("new.md").is_some() && fs1.read_file("old.md").is_none());
    assert!(fs1.read_file("small.md").is_some());
    assert!(fs1.read_file("big.bin").is_none());
    // Правка с другого устройства доходит до устройства с ждущим файлом.
    let t = edit_line(&mut w, 1, "keep.md", 1);
    w.sync(1);
    w.sync(0);
    assert!(
        String::from_utf8(read(&w, 0, "keep.md"))
            .unwrap()
            .contains(&t)
    );
    // Пока места нет, ждущий файл не отправляется каждый цикл.
    w.trace.clear();
    w.sync(0);
    assert!(!uploads_in_trace(&w), "повторная отправка без места");
    assert_eq!(quota_full_notices(&w, 0), 1, "одно уведомление");
    // Порог сняли — файл уходит в следующем цикле.
    set_hard(&w, None);
    w.sync(0);
    assert_eq!(quota_of(&w, 0).held, 0);
    w.sync(1);
    assert!(w.clients[1].fs.read_file("big.bin").is_some());
    finish(&mut w);
}

/// Освободить место из устройства: очистка корзины и стирание старых версий снимают
/// ожидание без админа.
#[test]
fn purging_trash_and_history_frees_room() {
    use selfsync_core::engine::{UiCommand, UiResult};
    let mut w = World::new(quiet(2));
    sized_file(&mut w, 0, "old.bin", 3000);
    w.sync(0);
    sized_file(&mut w, 0, "old.bin", 3000);
    w.sync(0);
    sized_file(&mut w, 0, "gone.bin", 3000);
    w.sync(0);
    w.user_delete(0, "gone.bin");
    w.sync(0);
    set_hard(&w, Some(server_used(&w) + 100));
    sized_file(&mut w, 0, "new.bin", 2500);
    w.sync(0);
    assert_eq!(quota_of(&w, 0).held, 1);
    let r = ui_result(
        &mut w,
        0,
        1,
        UiCommand::PurgeDeleted {
            paths: vec!["gone.bin".into()],
        },
    );
    assert_eq!(r, UiResult::Restored { count: 1 });
    let r = ui_result(
        &mut w,
        0,
        2,
        UiCommand::PurgeHistory {
            path: "old.bin".into(),
            before_rev: 2,
        },
    );
    assert_eq!(r, UiResult::Restored { count: 1 });
    w.sync(0);
    assert_eq!(quota_of(&w, 0).held, 0, "место освободилось — файл ушёл");
    w.sync(1);
    assert!(w.clients[1].fs.read_file("new.bin").is_some());
    // Устройство только для чтения не стирает.
    w.set_read_only(1);
    w.sync(1);
    let r = ui_result(
        &mut w,
        1,
        3,
        UiCommand::PurgeHistory {
            path: "new.bin".into(),
            before_rev: 9,
        },
    );
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "read_only"),
        "{r:?}"
    );
    finish(&mut w);
}

/// Лимит размера файла vault'а меньше лимита устройства: файл не отправляется, одно
/// уведомление; правка файла в пределах лимита — снова отправляется.
#[test]
fn vault_max_file_is_respected() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    {
        use selfsync_server::quota;
        let v = w.state.vault("sim").unwrap();
        let c = v.pool.get().unwrap();
        quota::set_own(
            &c,
            &quota::Own {
                max_file: Some(1500),
                ..quota::Own::default()
            },
        )
        .unwrap();
    }
    // Устройство ещё не знает лимита: отказ приходит на загрузку.
    sized_file(&mut w, 0, "big.bin", 2000);
    w.sync(0);
    // Теперь знает: такой файл и не читается для отправки.
    sized_file(&mut w, 0, "big2.bin", 2000);
    w.sync(0);
    w.sync(0);
    let n = w.clients[0]
        .notices
        .iter()
        .filter(|n| matches!(n, Notice::TooLarge { path, .. } if path == "big2.bin"))
        .count();
    assert_eq!(n, 1);
    w.user_delete(0, "big2.bin");
    w.sync(0);
    let too_large = w.clients[0]
        .notices
        .iter()
        .filter(|n| matches!(n, Notice::TooLarge { path, .. } if path == "big.bin"))
        .count();
    assert_eq!(too_large, 1);
    sized_file(&mut w, 0, "big.bin", 100);
    w.sync(0);
    w.sync(1);
    assert!(w.clients[1].fs.read_file("big.bin").is_some());
    finish(&mut w);
}

/// Событие переименования пришло поздно: синк уже вернул файл по старому пути (с
/// чужой правкой). Переехавший файл — новый, а не продолжение вернувшегося: иначе
/// переименование на сервере увело бы чужую правку, и её затёрли бы старым текстом.
#[test]
fn late_rename_event_after_restore_keeps_remote_edit() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "c.md");
    let t1 = edit_line(&mut w, 1, "c.md", 1);
    w.sync(1);
    let now = w.now;
    assert!(w.clients[0].fs.user_rename("c.md", "r.md", now));
    // Циклы по таймеру, без полного скана: синк узнаёт о пропаже c.md от сервера.
    for _ in 0..6 {
        w.now += 30_000;
        w.tick(0);
        w.drain(0);
    }
    let back = String::from_utf8(read(&w, 0, "c.md")).unwrap();
    assert!(back.contains(&t1), "c.md возвращён с правкой: {back}");
    w.deliver(
        0,
        Event::Renamed {
            from: "c.md".into(),
            to: "r.md".into(),
        },
    );
    w.sync(0);
    // Вернувшийся c.md на сервере не трогали: он не переименован и не создан заново.
    let rev: i64 = {
        use selfsync_core::path::{VaultPath, canonical_encode};
        let key = canonical_encode(&VaultPath::parse("c.md").unwrap().to_proto());
        let v = w.state.vault("sim").unwrap();
        let c = v.pool.get().unwrap();
        c.query_row("SELECT rev FROM files WHERE path = ?1", [key], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(rev, 2);
    w.sync(1);
    let c1 = String::from_utf8(read(&w, 1, "c.md")).unwrap();
    assert!(c1.contains(&t1), "{c1}");
    assert!(w.clients[1].fs.read_file("r.md").is_some());
    finish(&mut w);
}

/// Переименование упёрлось в чужую правку источника (ревизия запомнена для повтора,
/// содержимое ещё не сверено), потом имя вернули и файл поправили: Put по этой
/// ревизии затёр бы чужую правку — он идёт как по неизвестной базе, со слиянием.
#[test]
fn undone_rename_after_conflict_keeps_remote_edit() {
    let mut w = World::new(quiet(2));
    shared_note(&mut w, "a.md");
    let t1 = edit_line(&mut w, 1, "a.md", 1);
    w.sync(1);
    assert!(w.user_rename(0, "a.md", "b.md"));
    // Конфликт переименования записан, а дальше связь пропала: ни повтора
    // переименования, ни получения изменений — серверную правку устройство не видело.
    w.deliver(0, Event::SyncNow);
    while let Some(label) = w.pump_one(0) {
        if label == "HTTP POST /v1/ops" {
            break;
        }
    }
    w.planned_faults.push_back(("POST /v1/ops".into(), false));
    w.drain(0);
    assert!(w.planned_faults.is_empty(), "повтор переименования оборван");
    assert!(w.user_rename(0, "b.md", "a.md"));
    let t0 = edit_line(&mut w, 0, "a.md", 3);
    w.sync(0);
    w.sync(1);
    finish(&mut w);
    for c in 0..2 {
        let text = String::from_utf8(read(&w, c, "a.md")).unwrap_or_default();
        let copies: String = w.clients[c]
            .fs
            .snapshot()
            .into_iter()
            .filter(|(p, _)| p.starts_with("a ("))
            .map(|(_, b)| String::from_utf8(b).unwrap())
            .collect();
        assert!(text.contains(&t1) || copies.contains(&t1), "c{c}: {text}");
        assert!(text.contains(&t0) || copies.contains(&t0), "c{c}: {text}");
    }
}
