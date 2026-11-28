//! Card order. Cards are shown with open todos first, then the rest; inside each group by `position`, highest first.
//!
//! A position is a short string of base-62 digits (`0-9A-Za-z`) read as a fraction, so positions sort the same way as text
//! (byte order) and as numbers, in any store. There is always a position between two others, so moving a card changes
//! only that card: nothing is ever renumbered. A position never ends in `0`, which keeps that true. Positions stay short in
//! normal use; moving cards into the very same gap over and over lengthens them by about one character per five moves.
//!
//! Everything here is pure arithmetic on positions; reading and writing them is the storage backend's job.

const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: usize = 62;

fn digit(c: u8) -> usize {
    DIGITS.iter().position(|d| *d == c).unwrap_or(0)
}

/// A card as the ordering rules see it, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub pos: String,
    /// Has open checklist items, so it lives in the top group.
    pub pinned: bool,
    pub due: Option<i64>,
}

/// A string strictly between `a` and `b` (`a` may be empty, meaning the very bottom; `b` None means no upper limit).
/// Requires `a < b`, and that neither ends in `0`.
fn midpoint(a: &[u8], b: Option<&[u8]>) -> Vec<u8> {
    if let Some(b) = b {
        // Skip the digits they share (a missing digit of `a` counts as 0), then solve for what is left.
        let mut n = 0;
        while n < b.len() && *a.get(n).unwrap_or(&b'0') == b[n] {
            n += 1;
        }
        debug_assert!(n < b.len(), "a must be smaller than b");
        if n > 0 && n < b.len() {
            let mut out = b[..n].to_vec();
            out.extend(midpoint(a.get(n..).unwrap_or(&[]), Some(&b[n..])));
            return out;
        }
    }
    let da = a.first().map_or(0, |c| digit(*c));
    let db = b.map_or(BASE, |b| digit(b[0]));
    if db - da > 1 {
        vec![DIGITS[(da + db + 1) / 2]]
    } else if let Some(b) = b.filter(|b| b.len() > 1) {
        // adjacent first digits and `b` goes on: its first digit alone is between them
        vec![b[0]]
    } else {
        let mut out = vec![DIGITS[da]];
        out.extend(midpoint(a.get(1..).unwrap_or(&[]), None));
        out
    }
}

/// Just above `lo` with nothing above it: one step up in the last digit, or a longer key when that digit is already the
/// largest. Adding cards one after another on top therefore lengthens keys by one character per 60 cards.
fn key_after(lo: &str) -> Vec<u8> {
    match lo.as_bytes().split_last() {
        None => vec![DIGITS[BASE / 2]],
        Some((&last, rest)) if digit(last) + 1 < BASE => {
            let mut out = rest.to_vec();
            out.push(DIGITS[digit(last) + 1]);
            out
        }
        Some(_) => {
            // already the largest digit: add a digit, starting low so that it has room to count up again
            let mut out = lo.as_bytes().to_vec();
            out.push(DIGITS[1]);
            out
        }
    }
}

/// Just below `hi` with nothing below it, never ending in `0`: one step down in the last digit, or when that digit is `1`,
/// a `0` followed by the largest digit.
fn key_before(hi: &str) -> Vec<u8> {
    let (&last, rest) = hi.as_bytes().split_last().expect("a position is never empty");
    let mut out = rest.to_vec();
    if digit(last) > 1 {
        out.push(DIGITS[digit(last) - 1]);
    } else {
        out.extend([b'0', DIGITS[BASE - 1]]);
    }
    out
}

/// A position between `below` (the smaller; None for the bottom) and `above` (the larger; None for the top). If they
/// are not in that order (two cards that were given the same position by simultaneous moves), the result is simply
/// above `below`.
pub fn key_between(below: Option<&str>, above: Option<&str>) -> String {
    let lo = below.unwrap_or("");
    let out = match above.filter(|hi| lo < *hi) {
        None => key_after(lo),
        Some(hi) if below.is_none() => key_before(hi),
        Some(hi) => midpoint(lo.as_bytes(), Some(hi.as_bytes())),
    };
    String::from_utf8(out).expect("digits are ASCII")
}

/// `n` positions in increasing order, for numbering existing cards when positions are first created. All have the same
/// length and end in a non-zero digit.
pub fn spread(n: usize) -> Vec<String> {
    let mut width = 1;
    while BASE.pow(width as u32) < n + 1 {
        width += 1;
    }
    (0..n)
        .map(|i| {
            let mut v = i + 1;
            let mut s = vec![b'0'; width];
            for slot in s.iter_mut().rev() {
                *slot = DIGITS[v % BASE];
                v /= BASE;
            }
            s.push(DIGITS[BASE / 2]); // a fixed non-zero last digit keeps the "never ends in 0" rule
            String::from_utf8(s).expect("digits are ASCII")
        })
        .collect()
}

/// Where a new card goes, given the owner's cards in display order. Ordinary cards go on top of everything. A new open
/// todo is slotted by due date among the open todos as they are currently ordered: just above the first one due later
/// (or undated); undated todos go last.
pub fn place_new(all: &[Slot], open_todo: bool, due: Option<i64>) -> String {
    let top = all.iter().map(|x| x.pos.as_str()).max();
    if !open_todo {
        return key_between(top, None);
    }
    let group: Vec<&Slot> = all.iter().filter(|x| x.pinned).collect();
    if group.is_empty() {
        return key_between(top, None);
    }
    let later = |d: Option<i64>| match (due, d) {
        (Some(a), Some(b)) => b > a,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let idx = group.iter().position(|x| later(x.due)).unwrap_or(group.len());
    let above = if idx == 0 { None } else { Some(group[idx - 1].pos.as_str()) };
    let below = group.get(idx).map(|x| x.pos.as_str());
    key_between(below, above)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    NoNeighbour,
    OwnNeighbour,
    OtherGroup,
    WrongOrder,
}

impl MoveError {
    pub fn message(self) -> &'static str {
        match self {
            MoveError::NoNeighbour => "give the card above or below the new place",
            MoveError::OwnNeighbour => "a card cannot be its own neighbour",
            MoveError::OtherGroup => "open todo cards stay above the other cards; move it within its group",
            MoveError::WrongOrder => "the card above must come before the card below",
        }
    }
}

/// Checks a move request that needs no stored data. Call after confirming the card is the caller's.
pub fn check_request(id: &str, prev: Option<&str>, next: Option<&str>) -> Result<(), MoveError> {
    if prev.is_none() && next.is_none() {
        return Err(MoveError::NoNeighbour);
    }
    if prev == Some(id) || next == Some(id) {
        return Err(MoveError::OwnNeighbour);
    }
    Ok(())
}

/// Checks the neighbours (each a position and whether it is in the top group) against the card being moved: both must be
/// in its group, and the one above must really be above the one below.
pub fn check_neighbours(pinned: bool, above: Option<(&str, bool)>, below: Option<(&str, bool)>) -> Result<(), MoveError> {
    if above.iter().chain(below.iter()).any(|(_, p)| *p != pinned) {
        return Err(MoveError::OtherGroup);
    }
    if let (Some(a), Some(b)) = (above, below) {
        if a.0 < b.0 {
            return Err(MoveError::WrongOrder);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pos: &str, pinned: bool, due: Option<i64>) -> Slot {
        Slot { pos: pos.into(), pinned, due }
    }

    fn valid(k: &str) -> bool {
        !k.is_empty() && k.bytes().all(|c| DIGITS.contains(&c)) && !k.ends_with('0')
    }

    #[test]
    fn digits_sort_the_same_as_text() {
        assert!(DIGITS.windows(2).all(|w| w[0] < w[1]), "byte order is digit order");
    }

    #[test]
    fn keys_between_are_strictly_between() {
        assert_eq!(key_between(None, None), "V");
        assert_eq!(key_between(Some("V"), None), "W", "on top: one step up");
        assert_eq!(key_between(None, Some("V")), "U", "at the bottom: one step down");
        assert_eq!(key_between(Some("z"), None), "z1", "past the largest digit the key gets longer");
        assert_eq!(key_between(None, Some("1")), "0z", "never ends in 0");
        let mid = key_between(Some("V"), Some("W"));
        assert!(mid.as_str() > "V" && mid.as_str() < "W" && valid(&mid), "{mid}");
        // below the smallest possible single digit, and above the largest
        let tiny = key_between(None, Some("1"));
        assert!(tiny.as_str() < "1" && valid(&tiny), "{tiny}");
        let huge = key_between(Some("z"), None);
        assert!(huge.as_str() > "z" && valid(&huge), "{huge}");
        // prefixes: "a" < "aV" < "b"
        let k = key_between(Some("a"), Some("b"));
        assert!(k.as_str() > "a" && k.as_str() < "b", "{k}");
        let k = key_between(Some("a1"), Some("a2"));
        assert!(k.as_str() > "a1" && k.as_str() < "a2" && valid(&k), "{k}");
        let k = key_between(None, Some("01"));
        assert!(k.as_str() < "01" && valid(&k), "{k}");
    }

    #[test]
    fn equal_or_reversed_neighbours_still_give_a_valid_key_above_the_lower() {
        let k = key_between(Some("M"), Some("M"));
        assert!(k.as_str() > "M" && valid(&k));
        let k = key_between(Some("M"), Some("A"));
        assert!(k.as_str() > "M" && valid(&k));
    }

    /// Small deterministic generator, so the property tests need no crate.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    #[test]
    fn random_insertions_keep_a_strict_order_and_stay_short() {
        let mut rng = Lcg(42);
        let mut keys: Vec<String> = vec![];
        for _ in 0..3000 {
            // insert at a random place in the sorted list: bottom, top, or between two neighbours
            let at = rng.next(keys.len() + 1);
            let below = if at == 0 { None } else { Some(keys[at - 1].as_str()) };
            let above = keys.get(at).map(String::as_str);
            let k = key_between(below, above);
            assert!(valid(&k), "{k}");
            assert!(below.map_or(true, |b| b < k.as_str()) && above.map_or(true, |a| k.as_str() < a), "{below:?} < {k} < {above:?}");
            keys.insert(at, k);
        }
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert!(keys.iter().map(String::len).max().unwrap() < 40, "keys stay short under random use");
    }

    #[test]
    fn the_worst_cases_grow_slowly() {
        // always inserting at the same gap (the card dragged into the same place again and again)
        let (mut lo, hi) = ("V".to_string(), "W".to_string());
        for _ in 0..1000 {
            lo = key_between(Some(&lo), Some(&hi));
        }
        assert!(lo.len() <= 1000 / 5 + 10, "one character per five bisections of the same gap: {}", lo.len());
        // always adding on top, and always adding at the bottom
        let (mut top, mut bottom) = ("V".to_string(), "V".to_string());
        for _ in 0..1000 {
            top = key_between(Some(&top), None);
            bottom = key_between(None, Some(&bottom));
            assert!(valid(&top) && valid(&bottom));
        }
        assert!(top.len() <= 1000 / 60 + 3 && bottom.len() <= 1000 / 60 + 3, "one character per 60 cards: {} {}", top.len(), bottom.len());
    }

    #[test]
    fn spread_numbers_cards_in_order_without_trailing_zeros() {
        for n in [0, 1, 2, 61, 62, 63, 3843, 3844, 5000] {
            let keys = spread(n);
            assert_eq!(keys.len(), n);
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "n={n}");
            assert!(keys.iter().all(|k| valid(k)), "n={n}");
        }
        // there is room above and below the spread keys
        let keys = spread(100);
        assert!(key_between(None, Some(&keys[0])).as_str() < keys[0].as_str());
        assert!(key_between(keys.last().map(String::as_str), None).as_str() > keys[99].as_str());
    }

    #[test]
    fn ordinary_cards_go_above_everything() {
        let all = [s("c", true, Some(1)), s("b", false, None), s("a", false, None)];
        let k = place_new(&all, false, None);
        assert!(k.as_str() > "c" && valid(&k), "{k}");
        assert_eq!(place_new(&[], false, None), "V");
    }

    #[test]
    fn new_todos_are_slotted_by_due_date() {
        let all = [s("g", true, Some(100)), s("f", true, Some(200)), s("e", true, None), s("0V", false, None)];
        let k = place_new(&all, true, Some(150));
        assert!(k.as_str() < "g" && k.as_str() > "f", "just above the first one due later: {k}");
        let k = place_new(&all, true, Some(50));
        assert!(k.as_str() > "g", "before every other todo: {k}");
        let k = place_new(&all, true, Some(300));
        assert!(k.as_str() < "f" && k.as_str() > "e", "before the undated one: {k}");
        let k = place_new(&all, true, None);
        assert!(k.as_str() < "e", "undated go last in the group: {k}");
        let k = place_new(&[s("0V", false, None)], true, Some(1));
        assert!(k.as_str() > "0V", "the first todo goes on top: {k}");
    }

    #[test]
    fn move_rules() {
        assert_eq!(check_request("1", None, None), Err(MoveError::NoNeighbour));
        assert_eq!(check_request("1", Some("1"), None), Err(MoveError::OwnNeighbour));
        assert_eq!(check_request("1", None, Some("1")), Err(MoveError::OwnNeighbour));
        assert!(check_request("1", Some("2"), None).is_ok());
        assert_eq!(check_neighbours(false, Some(("e", true)), None), Err(MoveError::OtherGroup));
        assert_eq!(check_neighbours(true, None, Some(("e", false))), Err(MoveError::OtherGroup));
        assert_eq!(check_neighbours(false, Some(("b", false)), Some(("e", false))), Err(MoveError::WrongOrder));
        assert!(check_neighbours(false, Some(("e", false)), Some(("b", false))).is_ok());
        assert!(check_neighbours(true, None, None).is_ok());
    }
}
