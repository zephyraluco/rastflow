//! 单工作线程、单待执行槽位。新查询替换尚未执行的旧查询，防止输入积压。
use std::sync::{Arc, Condvar, Mutex};

struct State<T> {
    pending: Option<T>,
    stopped: bool,
}

pub struct LatestWorker<T> {
    state: Arc<(Mutex<State<T>>, Condvar)>,
}

impl<T: Send + 'static> LatestWorker<T> {
    pub fn new(name: &str, mut execute: impl FnMut(T) + Send + 'static) -> std::io::Result<Self> {
        let state = Arc::new((
            Mutex::new(State {
                pending: None,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let worker = Arc::clone(&state);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                loop {
                    let task = {
                        let (mutex, changed) = &*worker;
                        let mut guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
                        while guard.pending.is_none() && !guard.stopped {
                            guard = changed.wait(guard).unwrap_or_else(|e| e.into_inner());
                        }
                        if guard.stopped && guard.pending.is_none() {
                            break;
                        }
                        guard.pending.take().unwrap()
                    };
                    execute(task);
                }
            })?;
        Ok(Self { state })
    }

    /// 返回被替换的任务，由调用方完成取消及结果通知。
    pub fn submit(&self, task: T) -> Option<T> {
        let (mutex, changed) = &*self.state;
        let old = mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .replace(task);
        changed.notify_one();
        old
    }
}

impl<T> Drop for LatestWorker<T> {
    fn drop(&mut self) {
        let (mutex, changed) = &*self.state;
        mutex.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
        changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn running_task_does_not_allow_pending_queries_to_accumulate() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = LatestWorker::new("test-query", move |value| {
            if value == 1 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            done_tx.send(value).unwrap();
        })
        .unwrap();
        assert_eq!(worker.submit(1), None);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(worker.submit(2), None);
        assert_eq!(worker.submit(3), Some(2));
        release_tx.send(()).unwrap();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        drop(worker);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).is_err());
    }
}
