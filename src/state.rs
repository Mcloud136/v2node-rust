use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct UserEntry {
    pub uid: i32,
    pub speed_limit: i32,
    pub device_limit: i32,
}

#[derive(Default)]
pub struct UserStore {
    users: DashMap<Uuid, UserEntry>,
}

#[derive(Default)]
pub struct UserDiff {
    pub added: Vec<(Uuid, UserEntry)>,
    pub removed: Vec<(Uuid, UserEntry)>,
    pub modified: Vec<(Uuid, UserEntry)>,
}

impl UserStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, uuid: &Uuid) -> Option<UserEntry> {
        self.users.get(uuid).map(|entry| entry.clone())
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn apply(&self, users: Vec<(Uuid, UserEntry)>) -> UserDiff {
        let incoming: HashMap<Uuid, UserEntry> = users.into_iter().collect();
        let mut diff = UserDiff::default();
        let existing: Vec<(Uuid, UserEntry)> = self
            .users
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        for (uuid, old) in existing {
            match incoming.get(&uuid) {
                None => {
                    self.users.remove(&uuid);
                    diff.removed.push((uuid, old));
                }
                Some(new)
                    if new.uid != old.uid
                        || new.speed_limit != old.speed_limit
                        || new.device_limit != old.device_limit =>
                {
                    self.users.insert(uuid, new.clone());
                    diff.modified.push((uuid, new.clone()));
                }
                Some(_) => {}
            }
        }
        for (uuid, entry) in incoming {
            if !self.users.contains_key(&uuid) {
                self.users.insert(uuid, entry.clone());
                diff.added.push((uuid, entry));
            }
        }
        diff
    }

    pub fn all(&self) -> Vec<(Uuid, UserEntry)> {
        self.users
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect()
    }
}

pub struct TrafficStorage {
    pub up: AtomicU64,
    pub down: AtomicU64,
}

impl TrafficStorage {
    fn new() -> Self {
        Self {
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
        }
    }
}

#[derive(Default)]
pub struct TrafficCounters {
    counters: DashMap<Uuid, Arc<TrafficStorage>>,
}

impl TrafficCounters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn storage(&self, uuid: Uuid) -> Arc<TrafficStorage> {
        self.counters
            .entry(uuid)
            .or_insert_with(|| Arc::new(TrafficStorage::new()))
            .value()
            .clone()
    }

    pub fn add_up(&self, uuid: Uuid, bytes: usize) {
        self.storage(uuid)
            .up
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn add_down(&self, uuid: Uuid, bytes: usize) {
        self.storage(uuid)
            .down
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn take(&self, uuid: &Uuid) -> (u64, u64) {
        if let Some(storage) = self.counters.get(uuid) {
            let up = storage.up.swap(0, Ordering::Relaxed);
            let down = storage.down.swap(0, Ordering::Relaxed);
            (up, down)
        } else {
            (0, 0)
        }
    }

    pub fn remove(&self, uuid: &Uuid) {
        self.counters.remove(uuid);
    }
}

pub struct RateBucket {
    rate: f64,
    capacity: f64,
    state: Mutex<BucketState>,
}

struct BucketState {
    tokens: f64,
    last: Instant,
}

impl RateBucket {
    pub fn new(bytes_per_second: u64) -> Self {
        let rate = bytes_per_second as f64;
        let capacity = rate.max(65536.0);
        Self {
            rate,
            capacity,
            state: Mutex::new(BucketState {
                tokens: capacity,
                last: Instant::now(),
            }),
        }
    }

    pub async fn acquire(&self, bytes: usize) {
        if self.rate <= 0.0 || bytes == 0 {
            return;
        }
        let mut remaining = bytes as f64;
        while remaining > 0.0 {
            let needed = remaining.min(self.capacity);
            loop {
                let wait = {
                    let mut state = self.state.lock();
                    let now = Instant::now();
                    let elapsed = now.duration_since(state.last).as_secs_f64();
                    state.tokens = (state.tokens + elapsed * self.rate).min(self.capacity);
                    state.last = now;
                    if state.tokens >= needed {
                        state.tokens -= needed;
                        break;
                    }
                    (needed - state.tokens) / self.rate
                };
                sleep(Duration::from_secs_f64(wait.min(1.0))).await;
            }
            remaining -= needed;
        }
    }
}

#[derive(Default)]
pub struct Limiter {
    buckets: DashMap<Uuid, Arc<RateBucket>>,
    online: DashMap<Uuid, DashMap<IpAddr, i32>>,
    old_online: Mutex<HashMap<IpAddr, i32>>,
    alive: RwLock<HashMap<i32, i32>>,
    register_locks: DashMap<Uuid, Arc<Mutex<()>>>,
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_alive(&self, alive: HashMap<i32, i32>) {
        *self.alive.write() = alive;
    }

    pub fn check_and_register(&self, uuid: Uuid, uid: i32, ip: IpAddr, device_limit: i32) -> bool {
        let lock = self
            .register_locks
            .entry(uuid)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value()
            .clone();
        let _guard = lock.lock();
        let alive = self.alive.read().get(&uid).copied().unwrap_or(0);
        let ips = self.online.entry(uuid).or_default();
        if ips.contains_key(&ip) {
            return true;
        }
        let from_previous_window = {
            let mut old = self.old_online.lock();
            if old.get(&ip) == Some(&uid) {
                old.remove(&ip);
                true
            } else {
                false
            }
        };
        if !from_previous_window && device_limit > 0 && alive >= device_limit {
            return false;
        }
        ips.insert(ip, uid);
        true
    }

    pub fn bucket_for(&self, uuid: Uuid, speed_limit: i32) -> Option<Arc<RateBucket>> {
        if speed_limit <= 0 {
            return None;
        }
        let rate = speed_limit as u64 * 1_000_000 / 8;
        Some(
            self.buckets
                .entry(uuid)
                .or_insert_with(|| Arc::new(RateBucket::new(rate)))
                .value()
                .clone(),
        )
    }

    pub fn reset_bucket(&self, uuid: &Uuid) {
        self.buckets.remove(uuid);
    }

    pub fn remove_user(&self, uuid: &Uuid) {
        self.buckets.remove(uuid);
        self.online.remove(uuid);
        self.register_locks.remove(uuid);
    }

    pub fn take_online(&self) -> Vec<(Uuid, Vec<IpAddr>)> {
        let mut result = Vec::new();
        let mut previous = HashMap::new();
        let uuids: Vec<Uuid> = self.online.iter().map(|entry| *entry.key()).collect();
        for uuid in uuids {
            let lock = self
                .register_locks
                .entry(uuid)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .value()
                .clone();
            let _guard = lock.lock();
            let Some(entry) = self.online.get(&uuid) else {
                continue;
            };
            let ips: Vec<IpAddr> = entry.iter().map(|ip| *ip.key()).collect();
            for ip in &ips {
                if let Some(uid) = entry.get(ip) {
                    previous.insert(*ip, *uid);
                }
            }
            entry.clear();
            if !ips.is_empty() {
                result.push((uuid, ips));
            }
        }
        *self.old_online.lock() = previous;
        result
    }
}

struct LinkEntry {
    token: CancellationToken,
    client_ip: IpAddr,
}

#[derive(Default)]
pub struct LinkManager {
    next_id: AtomicU64,
    links: DashMap<Uuid, DashMap<u64, LinkEntry>>,
}

impl LinkManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        self: &Arc<Self>,
        uuid: Uuid,
        token: CancellationToken,
        client_ip: IpAddr,
    ) -> LinkRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.links
            .entry(uuid)
            .or_default()
            .insert(id, LinkEntry { token, client_ip });
        LinkRegistration {
            links: self.clone(),
            uuid,
            id,
        }
    }

    fn unregister(&self, uuid: Uuid, id: u64) {
        if let Some(user_links) = self.links.get(&uuid) {
            user_links.remove(&id);
        }
    }

    pub fn close_user(&self, uuid: &Uuid) {
        if let Some(user_links) = self.links.get(uuid) {
            for entry in user_links.iter() {
                entry.value().token.cancel();
            }
        }
    }

    pub fn close_all(&self) {
        for user_links in self.links.iter() {
            for entry in user_links.value().iter() {
                entry.value().token.cancel();
            }
        }
    }

    pub fn active_online(&self) -> HashMap<Uuid, Vec<IpAddr>> {
        let mut result = HashMap::new();
        for user_links in self.links.iter() {
            let mut ips = Vec::new();
            for entry in user_links.value().iter() {
                let ip = entry.value().client_ip;
                if !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
            if !ips.is_empty() {
                result.insert(*user_links.key(), ips);
            }
        }
        result
    }
}

pub struct LinkRegistration {
    links: Arc<LinkManager>,
    uuid: Uuid,
    id: u64,
}

impl Drop for LinkRegistration {
    fn drop(&mut self) {
        self.links.unregister(self.uuid, self.id);
    }
}

pub struct NodeState {
    pub users: Arc<UserStore>,
    pub counters: Arc<TrafficCounters>,
    pub limiter: Arc<Limiter>,
    pub links: Arc<LinkManager>,
}

impl NodeState {
    pub fn new() -> Self {
        Self {
            users: Arc::new(UserStore::new()),
            counters: Arc::new(TrafficCounters::new()),
            limiter: Arc::new(Limiter::new()),
            links: Arc::new(LinkManager::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_store_diff_add_modify_remove() {
        let store = UserStore::new();
        let uuid = Uuid::new_v4();
        let entry = UserEntry {
            uid: 1,
            speed_limit: 100,
            device_limit: 2,
        };
        let diff = store.apply(vec![(uuid, entry.clone())]);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(store.len(), 1);
        let modified = UserEntry {
            speed_limit: 200,
            ..entry.clone()
        };
        let diff = store.apply(vec![(uuid, modified)]);
        assert_eq!(diff.modified.len(), 1);
        let diff = store.apply(Vec::new());
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(store.len(), 0);
    }

    #[tokio::test]
    async fn rate_bucket_allows_small_bursts() {
        let bucket = RateBucket::new(1_000_000);
        bucket.acquire(1024).await;
    }

    #[test]
    fn active_online_reports_open_link_ips() {
        let links = Arc::new(LinkManager::new());
        let uuid = Uuid::new_v4();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let registration = links.register(uuid, CancellationToken::new(), ip);
        let online = links.active_online();
        assert_eq!(online.get(&uuid), Some(&vec![ip]));
        drop(registration);
        assert!(links.active_online().is_empty());
    }

    #[test]
    fn limiter_rejects_new_ip_over_device_limit() {
        let limiter = Limiter::new();
        let uuid = Uuid::new_v4();
        let mut alive = HashMap::new();
        alive.insert(1, 2);
        limiter.update_alive(alive);
        assert!(!limiter.check_and_register(uuid, 1, "203.0.113.7".parse().unwrap(), 2));
        assert!(limiter.check_and_register(uuid, 1, "203.0.113.7".parse().unwrap(), 3));
    }
}
