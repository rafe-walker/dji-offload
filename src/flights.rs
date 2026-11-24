//! Splitting a day's media into flights.
//!
//! A new flight starts whenever there is a gap longer than `gap` between two
//! files (landing, battery swap, take-off). Flights already recorded in the
//! ledger are extended rather than duplicated, so plugging the drone in after
//! every flight still yields Flight-1, Flight-2, ... for the day.

use chrono::{Duration, NaiveDate, NaiveDateTime};

#[derive(Debug, Clone, PartialEq)]
pub struct Flight {
    pub date: NaiveDate,
    pub num: u32,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// Folder name once chosen (kept stable across downloads).
    pub folder: Option<String>,
}

impl Flight {
    pub fn default_folder(&self) -> String {
        format!("Flight-{}_{}", self.num, self.start.format("%H%M"))
    }
}

/// Assign each capture time to a flight. `known` holds flights from earlier
/// downloads and is extended in place; new flights are appended.
/// Returns, for each input time (same order), the index into `known`.
pub fn assign(known: &mut Vec<Flight>, times: &[NaiveDateTime], gap: Duration) -> Vec<usize> {
    let mut order: Vec<usize> = (0..times.len()).collect();
    order.sort_by_key(|&i| times[i]);
    let mut out = vec![0usize; times.len()];
    for i in order {
        let t = times[i];
        let date = t.date();
        let hit = known
            .iter()
            .enumerate()
            .filter(|(_, f)| f.date == date && t >= f.start - gap && t <= f.end + gap)
            .min_by_key(|(_, f)| {
                if t < f.start { f.start - t } else if t > f.end { t - f.end } else { Duration::zero() }
            })
            .map(|(idx, _)| idx);
        let idx = match hit {
            Some(idx) => {
                let f = &mut known[idx];
                if t < f.start {
                    f.start = t;
                }
                if t > f.end {
                    f.end = t;
                }
                idx
            }
            None => {
                let num = known.iter().filter(|f| f.date == date).map(|f| f.num).max().unwrap_or(0) + 1;
                known.push(Flight { date, num, start: t, end: t, folder: None });
                known.len() - 1
            }
        };
        out[i] = idx;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn splits_on_gaps_and_days() {
        let mut known = vec![];
        let times = [
            t("2026-04-28 18:27:06"),
            t("2026-04-28 18:32:30"),
            t("2026-04-28 18:37:54"),
            t("2026-04-28 19:05:00"), // 27 min later -> flight 2
            t("2026-04-28 19:09:00"),
            t("2026-04-29 08:00:00"), // next day -> flight 1 of that day
        ];
        let a = assign(&mut known, &times, Duration::minutes(10));
        assert_eq!(a, vec![0, 0, 0, 1, 1, 2]);
        assert_eq!(known[1].num, 2);
        assert_eq!(known[1].default_folder(), "Flight-2_1905");
        assert_eq!(known[2].num, 1);
    }

    #[test]
    fn continues_across_downloads() {
        let mut known = vec![];
        assign(&mut known, &[t("2026-04-28 18:27:06"), t("2026-04-28 18:32:30"), t("2026-04-28 18:37:54")], Duration::minutes(10));
        known[0].folder = Some("Flight-1_1827_APN-999-12-345A".into());
        // Second plug-in: tail of flight 1 that was still recording, then flight 2.
        let a = assign(
            &mut known,
            &[t("2026-04-28 18:39:00"), t("2026-04-28 19:30:00")],
            Duration::minutes(10),
        );
        assert_eq!(a, vec![0, 1]);
        assert_eq!(known[0].end, t("2026-04-28 18:39:00"));
        assert_eq!(known[0].folder.as_deref(), Some("Flight-1_1827_APN-999-12-345A"));
        assert_eq!(known[1].num, 2);
    }
}
