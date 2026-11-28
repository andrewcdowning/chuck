//! Making new ids (see `chuck_core::domain::id` for their shape). Ids made by one process are strictly increasing, even
//! within the same millisecond, so they sort in creation order.

use chuck_core::domain::id;
use std::io::Read;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct State {
    last_ms: u64,
    last_random: u128,
    urandom: Option<std::fs::File>,
    fallback: u64,
}

static STATE: Mutex<State> = Mutex::new(State { last_ms: 0, last_random: 0, urandom: None, fallback: 0 });

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 80 random bits: from the operating system when it offers `/dev/urandom`, else from the per-process random seed of the
/// standard hasher mixed with a counter and the time. Ids need to be unique, not unguessable: who may see a card is decided
/// by its owner, never by knowing its id.
fn random80(st: &mut State) -> u128 {
    let mut buf = [0u8; 10];
    if st.urandom.is_none() {
        st.urandom = std::fs::File::open("/dev/urandom").ok();
    }
    if let Some(f) = st.urandom.as_mut() {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().fold(0u128, |a, b| (a << 8) | *b as u128);
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut out = 0u128;
    for _ in 0..2 {
        st.fallback = st.fallback.wrapping_add(1);
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(st.fallback);
        h.write_u64(now_ms());
        out = (out << 64) | h.finish() as u128;
    }
    out & ((1u128 << 80) - 1)
}

/// A new id stamped with the given time (milliseconds). Used when old data is given ids, so that they sort the way the
/// data was created.
pub fn new_id_at(ms: u64) -> String {
    let mut st = STATE.lock().unwrap();
    let random = random80(&mut st);
    id::encode(ms, random)
}

/// A new id for now.
pub fn new_id() -> String {
    let mut st = STATE.lock().unwrap();
    let ms = now_ms().max(st.last_ms);
    let random = if ms == st.last_ms {
        // same millisecond as the last one: count up, so ids still sort in the order they were made
        st.last_random.wrapping_add(1) & ((1u128 << 80) - 1)
    } else {
        random80(&mut st) >> 1 // leave headroom for counting up
    };
    st.last_ms = ms;
    st.last_random = random;
    id::encode(ms, random)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_valid_unique_and_increasing() {
        let ids: Vec<String> = (0..20_000).map(|_| new_id()).collect();
        assert!(ids.iter().all(|i| id::is_valid(i)));
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "strictly increasing, even inside one millisecond");
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    }

    #[test]
    fn ids_carry_the_time_they_were_made() {
        let before = now_ms();
        let i = new_id();
        let after = now_ms();
        let t = id::timestamp_ms(&i).unwrap();
        assert!(t >= before && t <= after + 1, "{before} {t} {after}");
        assert_eq!(id::timestamp_ms(&new_id_at(1_234_567)).unwrap(), 1_234_567);
    }

    #[test]
    fn ids_made_at_the_same_time_still_differ() {
        let a: HashSet<String> = (0..1000).map(|_| new_id_at(42)).collect();
        assert_eq!(a.len(), 1000);
    }

    #[test]
    fn threads_do_not_clash() {
        let handles: Vec<_> = (0..8).map(|_| std::thread::spawn(|| (0..5000).map(|_| new_id()).collect::<Vec<_>>())).collect();
        let all: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        assert_eq!(all.iter().collect::<HashSet<_>>().len(), all.len());
    }
}
