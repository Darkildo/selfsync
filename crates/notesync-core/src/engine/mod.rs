//! Sans-IO движок синка: `engine.handle(now, event) -> Vec<Action>`.
//!
//! Движок не делает ввода-вывода и не читает часы: время приходит с каждым событием,
//! а файлы, сеть и таймеры — через действия, которые выполняет исполнитель (TS в
//! плагине, tokio в CLI, фейковое окружение в симуляции). Поэтому одна логика
//! работает везде и детерминированно тестируется.

mod api;
mod commands;
mod crypto_flow;
mod ctx;
mod pull;
mod push;
mod resolve;
mod runtime;
mod scan;
mod sync;
mod transfer;
pub mod types;

use std::cell::RefCell;
use std::rc::Rc;

pub use ctx::{SyncError, TEMP_SUFFIX};
use ctx::{Ctx, State, build_excludes};
use runtime::{Hub, Task, poll_task};
pub use types::*;

use crate::index::Index;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sync,
    Wait,
    Unlock,
    Password,
    Resolve,
    Command,
}

pub struct Engine {
    hub: Rc<Hub>,
    st: Rc<RefCell<State>>,
    tasks: Vec<(Kind, Task)>,
    index_reset: bool,
}

impl Engine {
    /// Движок из сохранённого снимка индекса. Повреждённый снимок — безопасная
    /// первичная сверка (ничего локального не удаляется).
    pub fn new(config: EngineConfig, index: Option<&[u8]>) -> Engine {
        let (idx, reset) = match index {
            None => (Index::new(), false),
            Some(b) => match Index::decode(b) {
                Ok(i) => (i, false),
                Err(_) => (Index::new(), true),
            },
        };
        Engine {
            hub: Hub::new(),
            st: Rc::new(RefCell::new(State::new(config, idx))),
            tasks: Vec::new(),
            index_reset: reset,
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            hub: Rc::clone(&self.hub),
            st: Rc::clone(&self.st),
        }
    }

    /// Обрабатывает событие и возвращает действия для исполнителя.
    pub fn handle(&mut self, now: i64, event: Event) -> Vec<Action> {
        {
            let mut s = self.st.borrow_mut();
            // Часы клиента могут прыгать — внутреннее время только растёт.
            if now > s.now {
                s.now = now;
            }
        }
        self.on_event(event);
        self.run();
        self.hub.take_outbox()
    }

    fn spawn(&mut self, kind: Kind, f: impl std::future::Future<Output = ()> + 'static) {
        self.tasks.push((kind, Box::pin(f)));
    }

    fn running(&self, kind: Kind) -> bool {
        self.tasks.iter().any(|(k, _)| *k == kind)
    }

    fn on_event(&mut self, event: Event) {
        let cx = self.ctx();
        let now = cx.now();
        let debounce = cx.with(|s| i64::try_from(s.cfg.debounce_ms).unwrap_or(2500));
        let touch = |s: &mut State| {
            s.last_activity = now;
            s.poll_interval = s.cfg.poll_active_ms;
            s.sync_due = Some(now + debounce);
        };
        match event {
            Event::Start { key } => {
                cx.with_mut(|s| {
                    s.started = true;
                    s.need_full_scan = true;
                    s.sync_due = Some(now);
                    s.last_activity = now;
                });
                if let Some(k) = key {
                    crypto_flow::set_remembered(&cx, &k);
                }
                if self.index_reset {
                    self.index_reset = false;
                    cx.notify(Notice::IndexReset);
                }
                cx.set_status(|st| st.state = SyncState::Idle);
            }
            Event::Tick => {}
            Event::SyncNow => cx.with_mut(|s| {
                s.need_full_scan = true;
                s.sync_due = Some(now);
                s.backoff_ms = 0;
                s.wait_disabled = false;
                if s.blocked.as_deref().is_some_and(|b| b != "wrong_password" && b != "encryption_mismatch") {
                    s.blocked = None;
                }
                if s.paused.as_deref() == Some("encryption_started") {
                    s.paused = None;
                }
            }),
            Event::Changed { path } | Event::Deleted { path } => cx.with_mut(|s| {
                s.dirty.insert(path);
                touch(s);
            }),
            Event::Renamed { from, to } => cx.with_mut(|s| {
                s.renames.push((from, to));
                touch(s);
            }),
            Event::Visible => cx.with_mut(|s| {
                s.visible = true;
                s.need_full_scan = true;
                s.sync_due = Some(now);
                s.poll_interval = s.cfg.poll_active_ms;
            }),
            Event::Hidden => cx.with_mut(|s| {
                s.visible = false;
                s.next_poll = None;
                // На мобильных приложение вот-вот усыпят: сбросить очередь сейчас.
                if !s.dirty.is_empty() || !s.renames.is_empty() || s.index.pending_count() > 0 {
                    s.sync_due = Some(now);
                }
            }),
            Event::Done { id, result } => {
                self.hub.deliver(id, result);
            }
            Event::Password { password, remember } => {
                if !self.running(Kind::Unlock) {
                    let c = cx.clone();
                    self.spawn(Kind::Unlock, async move {
                        if let Err(e) = crypto_flow::unlock(&c, password, remember).await {
                            c.log(LogLevel::Warn, format!("пароль: {e}"));
                            c.notify(Notice::Error {
                                code: "unlock_failed".into(),
                                message: e.to_string(),
                            });
                        }
                    });
                }
            }
            Event::EnableEncryption { password, remember } => cx.with_mut(|s| {
                s.enable = Some((password, remember));
                s.sync_due = Some(now);
            }),
            Event::ChangePassword { old, new } => {
                let c = cx.clone();
                self.spawn(Kind::Password, async move {
                    if let Err(e) = crypto_flow::change_password(&c, old, new).await {
                        c.notify(Notice::Error {
                            code: "change_password_failed".into(),
                            message: e.to_string(),
                        });
                    }
                });
            }
            Event::Resolve { id, choice } => {
                let c = cx.clone();
                self.spawn(Kind::Resolve, async move {
                    if let Err(e) = commands::resolve_choice(&c, id, choice).await {
                        c.log(LogLevel::Warn, format!("решение конфликта: {e}"));
                    }
                });
            }
            Event::Command { req, command } => {
                let c = cx.clone();
                self.spawn(Kind::Command, async move { commands::run(&c, req, command).await });
            }
            Event::Configure { config } => cx.with_mut(|s| {
                s.excludes = build_excludes(&config);
                s.cfg = config;
                s.need_full_scan = true;
                s.sync_due = Some(now);
                s.wait_disabled = false;
            }),
        }
    }

    /// Крутит задачи, пока они продвигаются; планирует новые циклы.
    fn run(&mut self) {
        for _ in 0..64 {
            let mut progressed = false;
            let mut i = 0;
            while i < self.tasks.len() {
                if poll_task(&mut self.tasks[i].1) {
                    drop(self.tasks.swap_remove(i));
                    progressed = true;
                } else {
                    i += 1;
                }
            }
            if self.schedule() {
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        self.emit_wake();
    }

    /// Запускает цикл синка или long-poll, если пора. Возвращает, запущено ли что-то.
    fn schedule(&mut self) -> bool {
        let cx = self.ctx();
        let (started, blocked, paused_pw, now, due, next_poll) = cx.with(|s| {
            (
                s.started,
                s.blocked.is_some(),
                s.paused.as_deref() == Some("need_password") && s.keys.is_none(),
                s.now,
                s.sync_due,
                s.next_poll,
            )
        });
        if !started || blocked {
            return false;
        }
        let mut spawned = false;
        let due_now = due.is_some_and(|d| d <= now) || next_poll.is_some_and(|p| p <= now);
        if due_now && !self.running(Kind::Sync) && !paused_pw {
            let c = cx.clone();
            cx.with_mut(|s| {
                s.sync_due = None;
                s.next_poll = None;
                s.sync_running = true;
            });
            self.spawn(Kind::Sync, async move { sync_task(c).await });
            spawned = true;
        }
        let want_wait = cx.with(|s| {
            s.cfg.use_wait && !s.wait_disabled && s.visible && s.index.initial_done && s.paused.is_none() && !s.sync_running
        });
        if want_wait && !self.running(Kind::Wait) && !self.running(Kind::Sync) {
            let c = cx.clone();
            self.spawn(Kind::Wait, async move { wait_task(c).await });
            spawned = true;
        }
        spawned
    }

    fn emit_wake(&mut self) {
        let wake = self.st.borrow().sync_due.into_iter().chain(self.st.borrow().next_poll).min();
        let changed = {
            let mut s = self.st.borrow_mut();
            if s.wake_at != wake {
                s.wake_at = wake;
                true
            } else {
                false
            }
        };
        if changed
            && let Some(at) = wake {
                self.hub.emit(Action::Wake { at });
            }
    }

    /// Текущий статус.
    pub fn status(&self) -> SyncStatus {
        self.st.borrow().status.clone()
    }

    /// Снимок индекса (для тестов и диагностики).
    pub fn index(&self) -> Index {
        self.st.borrow().index.clone()
    }

    /// Ничего не выполняется и не ожидает ответа (кроме long-poll).
    pub fn is_idle(&self) -> bool {
        self.tasks.iter().all(|(k, _)| *k == Kind::Wait)
    }

    /// Когда движок хочет следующий `Tick`.
    pub fn next_wake(&self) -> Option<i64> {
        self.st.borrow().wake_at
    }

    /// Есть ли локальные изменения, ещё не отправленные на сервер.
    pub fn has_pending(&self) -> bool {
        let s = self.st.borrow();
        !s.dirty.is_empty() || !s.renames.is_empty() || s.index.pending_count() > 0
    }
}

async fn sync_task(cx: Ctx) {
    let res = sync::run_cycle(&cx).await;
    let now = cx.now();
    let mut notice = None;
    cx.with_mut(|s| {
        s.sync_running = false;
        let more_local = !s.dirty.is_empty() || !s.renames.is_empty();
        match &res {
            Ok(changed) => {
                s.backoff_ms = 0;
                s.paused = None;
                s.status.state = SyncState::Idle;
                s.status.reason = None;
                s.status.last_sync = now;
                let recent = now - s.last_activity < 120_000;
                if *changed || recent {
                    s.poll_interval = s.cfg.poll_active_ms;
                } else {
                    s.poll_interval = (s.poll_interval.saturating_mul(2)).min(s.cfg.poll_idle_max_ms).max(s.cfg.poll_active_ms);
                }
            }
            Err(e) => {
                let (state, reason, retry) = match e {
                    SyncError::Network(_) => (SyncState::Offline, "offline".to_owned(), true),
                    SyncError::Server { .. } => (SyncState::Error, "server_error".to_owned(), true),
                    SyncError::Unauthorized => {
                        s.blocked = Some("unauthorized".into());
                        notice = Some(Notice::Unauthorized);
                        (SyncState::Blocked, "unauthorized".to_owned(), false)
                    }
                    SyncError::ProtoUnsupported(v) => {
                        s.blocked = Some("proto_unsupported".into());
                        notice = Some(Notice::ProtocolUnsupported { supported: *v });
                        (SyncState::Blocked, "proto_unsupported".to_owned(), false)
                    }
                    SyncError::Paused(r) => {
                        s.paused = Some(r.clone());
                        let st = if r == "need_password" {
                            SyncState::NeedPassword
                        } else {
                            SyncState::Idle
                        };
                        (st, r.clone(), false)
                    }
                    SyncError::Blocked(r) => {
                        s.blocked = Some(r.clone());
                        (SyncState::Blocked, r.clone(), false)
                    }
                    other => (SyncState::Error, format!("{other}"), true),
                };
                s.status.state = state;
                s.status.reason = Some(reason);
                if retry {
                    s.backoff_ms = (s.backoff_ms.saturating_mul(2)).clamp(5_000, 300_000);
                    s.sync_due = Some(now + i64::try_from(s.backoff_ms).unwrap_or(300_000));
                }
            }
        }
        if s.blocked.is_none() {
            if more_local && s.sync_due.is_none() {
                s.sync_due = Some(now + i64::try_from(s.cfg.debounce_ms).unwrap_or(2500));
            }
            // Опрос — только пока приложение на переднем плане.
            if s.visible {
                let interval = if s.cfg.use_wait && !s.wait_disabled {
                    s.cfg.poll_idle_max_ms
                } else {
                    s.poll_interval
                };
                s.next_poll = Some(now + i64::try_from(interval).unwrap_or(i64::MAX / 2));
            }
        }
    });
    if let Err(e) = &res
        && !matches!(e, SyncError::Network(_) | SyncError::Paused(_)) {
            cx.log(LogLevel::Warn, format!("синк: {e}"));
        }
    if let Some(n) = notice {
        cx.notify(n);
    }
    cx.set_status(|_| {});
}

/// Long-poll `/v1/wait`: будит синк, как только на сервере что-то изменилось. Если
/// сервер отвечает мгновенно (CGI), переключается на обычный опрос.
async fn wait_task(cx: Ctx) {
    loop {
        let (go, since, t0) = cx.with(|s| (s.visible && s.cfg.use_wait && !s.wait_disabled && s.blocked.is_none(), s.index.last_seq, s.now));
        if !go {
            return;
        }
        match api::wait(&cx, since, 25).await {
            Ok(vs) => {
                let now = cx.now();
                if vs.seq > since {
                    cx.with_mut(|s| s.sync_due = Some(now));
                    return;
                }
                if now - t0 < 1000 {
                    cx.with_mut(|s| s.wait_disabled = true);
                    return;
                }
            }
            Err(_) => {
                cx.with_mut(|s| s.wait_disabled = true);
                return;
            }
        }
    }
}
