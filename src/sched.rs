// 串行队列：豆包只有一个页签，出口必须排队。用 Condvar 保证严格 FIFO。
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

pub struct Item {
    pub seq: u64,
    pub prompt: String,
    pub enqueued_at: u64,
}

pub struct RunInfo {
    pub seq: u64,
    pub prompt: String,
    pub started_at: u64,
}

pub struct Sched {
    pub items: VecDeque<Item>,
    pub running: Option<RunInfo>,
    pub done: u64,
    pub fail: u64,
    pub last_cost: f64,
    pub seq: u64,
}

static SCHED: OnceLock<(Mutex<Sched>, Condvar)> = OnceLock::new();

fn sched() -> &'static (Mutex<Sched>, Condvar) {
    SCHED.get_or_init(|| {
        (
            Mutex::new(Sched {
                items: VecDeque::new(),
                running: None,
                done: 0,
                fail: 0,
                last_cost: 0.0,
                seq: 0,
            }),
            Condvar::new(),
        )
    })
}

pub struct Ticket {
    pub seq: u64,
    pub ahead: usize,
}

/// 入队。队列满返回 Err(())，调用方回 429。
pub fn enqueue(prompt: &str, max: usize) -> Result<Ticket, ()> {
    let (lock, _) = sched();
    let mut g = lock.lock().unwrap();
    if g.items.len() >= max {
        g.fail += 1;
        return Err(());
    }
    let ahead = g.items.len() + if g.running.is_some() { 1 } else { 0 };
    g.seq += 1;
    let seq = g.seq;
    g.items.push_back(Item {
        seq,
        prompt: prompt.chars().take(40).collect(),
        enqueued_at: crate::util::now_ms(),
    });
    Ok(Ticket { seq, ahead })
}

/// 阻塞到轮到自己。超过 wait_ms 返回 Err("QUEUE_TIMEOUT") 并把自己从队列摘掉。
pub fn acquire(seq: u64, wait_ms: u64) -> Result<(), &'static str> {
    let (lock, cv) = sched();
    let mut g = lock.lock().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms);

    loop {
        let front = g.items.front().map(|i| i.seq);
        if g.running.is_none() && front == Some(seq) {
            if let Some(item) = g.items.pop_front() {
                g.running = Some(RunInfo {
                    seq: item.seq,
                    prompt: item.prompt,
                    started_at: crate::util::now_ms(),
                });
            }
            return Ok(());
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            g.items.retain(|i| i.seq != seq);
            g.fail += 1;
            cv.notify_all();
            return Err("QUEUE_TIMEOUT");
        }
        let (ng, _t) = cv.wait_timeout(g, deadline - now).unwrap();
        g = ng;
    }
}

pub fn fail_now() {
    let (lock, cv) = sched();
    let mut g = lock.lock().unwrap();
    g.fail += 1;
    cv.notify_all();
}

pub fn release(ok: bool, cost: f64) {
    let (lock, cv) = sched();
    let mut g = lock.lock().unwrap();
    g.running = None;
    if ok {
        g.done += 1;
    } else {
        g.fail += 1;
    }
    g.last_cost = cost;
    cv.notify_all();
}

pub fn snapshot() -> Value {
    let (lock, _) = sched();
    let g = lock.lock().unwrap();
    let now = crate::util::now_ms();
    let running = match &g.running {
        Some(r) => json!({
            "seq": r.seq,
            "prompt": r.prompt,
            "startedAt": r.started_at,
            "elapsed": (now.saturating_sub(r.started_at)) / 1000,
        }),
        None => Value::Null,
    };
    let waiting: Vec<Value> = g
        .items
        .iter()
        .map(|i| {
            json!({
                "seq": i.seq,
                "prompt": i.prompt,
                "waited": (now.saturating_sub(i.enqueued_at)) / 1000,
            })
        })
        .collect();
    json!({
        "ok": true,
        "running": running,
        "waiting": waiting,
        "waitingCount": g.items.len(),
        "done": g.done,
        "fail": g.fail,
        "lastCost": g.last_cost,
    })
}
