//! Reminder, repeat and due-date fields of a card: how an update merges with what is stored.

pub const REPEATS: [&str; 4] = ["daily", "weekdays", "weekly", "monthly"];

/// The schedule-related fields of a card.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Schedule {
    pub remind_at: Option<i64>,
    pub repeat: Option<String>,
    pub reminder_done: bool,
    pub due_at: Option<i64>,
}

/// What a client sent for these fields. `None` means absent.
#[derive(Debug, Clone, Default)]
pub struct ScheduleInput<'a> {
    pub remind_at: Option<i64>,
    pub repeat: Option<&'a str>,
    pub reminder_done: Option<bool>,
    pub due_at: Option<i64>,
}

fn positive(v: Option<i64>) -> Option<i64> {
    v.filter(|v| *v > 0)
}

/// A new card: a value of 0 or less, or an empty repeat, means "none". A new reminder is never already done.
pub fn for_new(i: &ScheduleInput) -> Schedule {
    Schedule {
        remind_at: positive(i.remind_at),
        repeat: i.repeat.filter(|r| !r.is_empty()).map(String::from),
        reminder_done: false,
        due_at: positive(i.due_at),
    }
}

/// An update: an absent field is unchanged; 0 or less (or an empty repeat) clears it; a new reminder time re-arms a
/// finished reminder, while the same time leaves it as it was; `reminder_done` set explicitly always wins.
pub fn merged(cur: &Schedule, i: &ScheduleInput) -> Schedule {
    let remind_at = match i.remind_at {
        None => cur.remind_at,
        Some(v) if v <= 0 => None,
        Some(v) => Some(v),
    };
    let repeat = match i.repeat {
        None => cur.repeat.clone(),
        Some("") => None,
        Some(r) => Some(r.to_string()),
    };
    let reminder_done = i.reminder_done.unwrap_or(if remind_at != cur.remind_at { false } else { cur.reminder_done });
    let due_at = match i.due_at {
        None => cur.due_at,
        Some(v) if v <= 0 => None,
        Some(v) => Some(v),
    };
    Schedule { remind_at, repeat, reminder_done, due_at }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur() -> Schedule {
        Schedule { remind_at: Some(100), repeat: Some("weekly".into()), reminder_done: false, due_at: Some(50) }
    }

    #[test]
    fn new_cards_drop_empty_values() {
        let s = for_new(&ScheduleInput { remind_at: Some(0), repeat: Some(""), reminder_done: Some(true), due_at: Some(-3) });
        assert_eq!(s, Schedule::default(), "non-positive times and an empty repeat mean none; a new reminder is not done");
        let s = for_new(&ScheduleInput { remind_at: Some(5), repeat: Some("daily"), due_at: Some(9), ..Default::default() });
        assert_eq!((s.remind_at, s.repeat.as_deref(), s.due_at), (Some(5), Some("daily"), Some(9)));
    }

    #[test]
    fn absent_means_unchanged_and_nonpositive_clears() {
        assert_eq!(merged(&cur(), &ScheduleInput::default()), cur());
        let c = merged(&cur(), &ScheduleInput { remind_at: Some(-5), repeat: Some(""), due_at: Some(0), ..Default::default() });
        assert_eq!(c, Schedule::default());
    }

    #[test]
    fn a_new_time_rearms_and_the_same_time_does_not() {
        let done = Schedule { reminder_done: true, ..cur() };
        assert!(merged(&done, &ScheduleInput { remind_at: Some(100), ..Default::default() }).reminder_done);
        assert!(!merged(&done, &ScheduleInput { remind_at: Some(200), ..Default::default() }).reminder_done);
        assert!(!merged(&done, &ScheduleInput { remind_at: Some(0), ..Default::default() }).reminder_done, "clearing the time also resets it");
        assert!(merged(&cur(), &ScheduleInput { reminder_done: Some(true), ..Default::default() }).reminder_done);
        assert!(!merged(&done, &ScheduleInput { remind_at: Some(200), reminder_done: Some(false), ..Default::default() }).reminder_done);
        assert!(merged(&done, &ScheduleInput { remind_at: Some(200), reminder_done: Some(true), ..Default::default() }).reminder_done, "explicit wins");
    }
}
