//! Card order. Cards are shown with open todos first, then the rest; inside each group by `position`, highest first.
//! Positions are numbers (today floats). Everything here is arithmetic on positions; reading and writing them is the
//! storage backend's job.

/// A card as the ordering rules see it, in display order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub pos: f64,
    /// Has open checklist items, so it lives in the top group.
    pub pinned: bool,
    pub due: Option<i64>,
}

/// A position between two neighbours (either may be missing). None if they are too close to split.
pub fn between(above: Option<f64>, below: Option<f64>) -> Option<f64> {
    let p = match (above, below) {
        (Some(a), Some(b)) => (a + b) / 2.0,
        (Some(a), None) => a - 1.0,
        (None, Some(b)) => b + 1.0,
        (None, None) => 1.0,
    };
    let ok = above.map_or(true, |a| a - p > 1e-9) && below.map_or(true, |b| p - b > 1e-9);
    ok.then_some(p)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Place {
    At(f64),
    /// The gap is used up: spread all positions with `renumbered`, then ask again.
    Renumber,
}

/// The position after renumbering, for the card at index `k` (0 = first shown) of `n`: whole numbers, highest first.
pub fn renumbered(n: usize, k: usize) -> f64 {
    (n - k) as f64
}

/// Where a new card goes, given the owner's cards in display order. Ordinary cards go on top of the rest. A new open
/// todo is slotted by due date among the open todos as they are currently ordered: just above the first one due
/// later (or undated); undated todos go last.
pub fn place_new(all: &[Slot], open_todo: bool, due: Option<i64>) -> Place {
    let top = all.iter().map(|x| x.pos).fold(0.0_f64, f64::max);
    if !open_todo {
        let rest_top = all.iter().filter(|x| !x.pinned).map(|x| x.pos).fold(f64::MIN, f64::max);
        return Place::At(if rest_top == f64::MIN { top + 1.0 } else { rest_top.max(top) + 1.0 });
    }
    let group: Vec<_> = all.iter().filter(|x| x.pinned).collect();
    if group.is_empty() {
        return Place::At(top + 1.0);
    }
    let later = |d: Option<i64>| match (due, d) {
        (Some(a), Some(b)) => b > a,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let idx = group.iter().position(|x| later(x.due)).unwrap_or(group.len());
    let above = if idx == 0 { None } else { Some(group[idx - 1].pos) };
    let below = group.get(idx).map(|x| x.pos);
    match between(above, below) {
        Some(p) => Place::At(p),
        None => Place::Renumber,
    }
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
pub fn check_request(id: i64, prev: Option<i64>, next: Option<i64>) -> Result<(), MoveError> {
    if prev.is_none() && next.is_none() {
        return Err(MoveError::NoNeighbour);
    }
    if prev == Some(id) || next == Some(id) {
        return Err(MoveError::OwnNeighbour);
    }
    Ok(())
}

/// Checks the neighbours against the card being moved: both must be in its group, and the one above must really be
/// above the one below.
pub fn check_neighbours(pinned: bool, above: Option<(f64, bool)>, below: Option<(f64, bool)>) -> Result<(), MoveError> {
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

    fn s(pos: f64, pinned: bool, due: Option<i64>) -> Slot {
        Slot { pos, pinned, due }
    }

    #[test]
    fn between_splits_gaps_and_gives_up_when_too_close() {
        assert_eq!(between(Some(4.0), Some(2.0)), Some(3.0));
        assert_eq!(between(Some(4.0), None), Some(3.0));
        assert_eq!(between(None, Some(2.0)), Some(3.0));
        assert_eq!(between(None, None), Some(1.0));
        assert_eq!(between(Some(1.0 + 1e-10), Some(1.0)), None);
        let mut a = 2.0;
        let mut steps = 0;
        while let Some(p) = between(Some(a), Some(1.0)) {
            a = p;
            steps += 1;
            assert!(steps < 100);
        }
        assert!((25..60).contains(&steps), "halving runs out of precision after a few dozen moves: {steps}");
    }

    #[test]
    fn ordinary_cards_go_above_the_rest_and_below_open_todos_never_above() {
        let all = [s(9.0, true, Some(1)), s(5.0, false, None), s(4.0, false, None)];
        assert_eq!(place_new(&all, false, None), Place::At(10.0), "above everything, as the old code did");
        assert_eq!(place_new(&[], false, None), Place::At(1.0));
        assert_eq!(place_new(&[s(3.0, false, None)], false, None), Place::At(4.0));
    }

    #[test]
    fn new_todos_are_slotted_by_due_date() {
        let all = [s(9.0, true, Some(100)), s(8.0, true, Some(200)), s(7.0, true, None), s(3.0, false, None)];
        assert_eq!(place_new(&all, true, Some(150)), Place::At(8.5), "just above the first one due later");
        assert_eq!(place_new(&all, true, Some(50)), Place::At(10.0), "before every other todo");
        assert_eq!(place_new(&all, true, Some(300)), Place::At(7.5), "before the undated one");
        assert_eq!(place_new(&all, true, None), Place::At(6.0), "undated go last in the group");
        assert_eq!(place_new(&[s(3.0, false, None)], true, Some(1)), Place::At(4.0), "first todo goes on top");
    }

    #[test]
    fn a_used_up_gap_asks_for_renumbering() {
        let all = [s(2.0 + 1e-10, true, Some(1)), s(2.0, true, Some(9))];
        assert_eq!(place_new(&all, true, Some(5)), Place::Renumber);
        assert_eq!((renumbered(3, 0), renumbered(3, 2)), (3.0, 1.0));
    }

    #[test]
    fn move_rules() {
        assert_eq!(check_request(1, None, None), Err(MoveError::NoNeighbour));
        assert_eq!(check_request(1, Some(1), None), Err(MoveError::OwnNeighbour));
        assert_eq!(check_request(1, None, Some(1)), Err(MoveError::OwnNeighbour));
        assert!(check_request(1, Some(2), None).is_ok());
        assert_eq!(check_neighbours(false, Some((5.0, true)), None), Err(MoveError::OtherGroup));
        assert_eq!(check_neighbours(true, None, Some((5.0, false))), Err(MoveError::OtherGroup));
        assert_eq!(check_neighbours(false, Some((2.0, false)), Some((5.0, false))), Err(MoveError::WrongOrder));
        assert!(check_neighbours(false, Some((5.0, false)), Some((2.0, false))).is_ok());
        assert!(check_neighbours(true, None, None).is_ok());
    }
}
