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
