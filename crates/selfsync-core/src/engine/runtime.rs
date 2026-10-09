//! Мини-исполнитель: логика синка написана последовательным `async`-кодом, а каждая
//! операция ввода-вывода — это `Action` с id и future, которая завершается, когда
//! исполнитель вернёт `Event::Done` с тем же id.
//!
//! Снаружи движок остаётся sans-IO конечным автоматом (`handle(Event) -> Vec<Action>`):
//! нет потоков, нет рантайма, нет часов — одинаково работает в WASM, нативно и в
//! детерминированной симуляции.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use super::types::{Action, IoResult};

#[derive(Default)]
pub(crate) struct Hub {
    next_id: Cell<u64>,
    outbox: RefCell<Vec<Action>>,
    results: RefCell<BTreeMap<u64, IoResult>>,
    wakers: RefCell<BTreeMap<u64, Waker>>,
    /// id действий, на которые ещё ждут ответа.
    pending: RefCell<BTreeMap<u64, ()>>,
}

impl Hub {
    pub fn new() -> Rc<Hub> {
        let h = Hub::default();
        h.next_id.set(1);
        Rc::new(h)
    }

    pub fn alloc_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// Действие без ответа.
    pub fn emit(&self, a: Action) {
        self.outbox.borrow_mut().push(a);
    }

    /// Действие с ответом: возвращает future результата.
    pub fn submit(self: &Rc<Self>, make: impl FnOnce(u64) -> Action) -> IoFuture {
        let id = self.alloc_id();
        self.pending.borrow_mut().insert(id, ());
        self.outbox.borrow_mut().push(make(id));
        IoFuture {
            hub: Rc::clone(self),
            id,
        }
    }

    /// Доставляет результат. Неизвестный id (ответ на действие убитого процесса или
    /// повтор) игнорируется.
    pub fn deliver(&self, id: u64, r: IoResult) -> bool {
        if self.pending.borrow_mut().remove(&id).is_none() {
            return false;
        }
        self.results.borrow_mut().insert(id, r);
        if let Some(w) = self.wakers.borrow_mut().remove(&id) {
            w.wake();
        }
        true
    }

    pub fn take_outbox(&self) -> Vec<Action> {
        std::mem::take(&mut *self.outbox.borrow_mut())
    }
}

pub(crate) struct IoFuture {
    hub: Rc<Hub>,
    id: u64,
}

impl Future for IoFuture {
    type Output = IoResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult> {
        if let Some(r) = self.hub.results.borrow_mut().remove(&self.id) {
            return Poll::Ready(r);
        }
        self.hub
            .wakers
            .borrow_mut()
            .insert(self.id, cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for IoFuture {
    fn drop(&mut self) {
        // Future брошена (задача отменена): ответ больше не нужен.
        self.hub.pending.borrow_mut().remove(&self.id);
        self.hub.wakers.borrow_mut().remove(&self.id);
        self.hub.results.borrow_mut().remove(&self.id);
    }
}

pub(crate) type Task = Pin<Box<dyn Future<Output = ()>>>;

/// Опрашивает задачу один раз. Верхний уровень опрашивается после каждого события,
/// поэтому ему достаточно noop-вейкера; вложенные комбинаторы (buffer_unordered)
/// получают свои вейкеры от `IoFuture`.
pub(crate) fn poll_task(t: &mut Task) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    t.as_mut().poll(&mut cx).is_ready()
}
