//! Local daily word counts and spoken time. Transcript text is never persisted.
use crate::settings::{atomic_write, config_dir};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::PathBuf, sync::OnceLock, time::Duration};
use time::{Date, OffsetDateTime, UtcOffset};

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Captures the local offset before Linux starts threads; falls back to UTC.
pub fn init_local_offset() {
    LOCAL_OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

/// Today's date in the offset captured at startup (UTC if uninitialized).
pub fn today() -> Date {
    OffsetDateTime::now_utc()
        .to_offset(LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC))
        .date()
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Day {
    words: u64,
    dictations: u64,
    spoken_ms: u64,
}

/// Daily aggregates keyed by local ISO date, retained for lifetime totals.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Stats {
    days: BTreeMap<String, Day>,
}

/// Lifetime totals and a zero-filled 30-day word series ending today.
pub struct Summary {
    pub total_words: u64,
    pub total_dictations: u64,
    pub average_wpm: u64,
    pub words_today: u64,
    pub daily: [u64; 30],
}

impl Stats {
    /// Computes speed from total words and total spoken time, rounded to WPM.
    pub fn summary(&self, today: Date) -> Summary {
        let total_words = self.days.values().map(|day| day.words).sum();
        let total_dictations = self.days.values().map(|day| day.dictations).sum();
        let spoken_ms: u64 = self.days.values().map(|day| day.spoken_ms).sum();
        let daily = std::array::from_fn(|i| {
            let date = today - time::Duration::days(29 - i as i64);
            self.days.get(&date.to_string()).map_or(0, |day| day.words)
        });
        Summary {
            total_words,
            total_dictations,
            average_wpm: if spoken_ms == 0 {
                0
            } else {
                (total_words as f64 * 60_000.0 / spoken_ms as f64).round() as u64
            },
            words_today: daily[29],
            daily,
        }
    }
}

/// UI-thread stats value and its atomic JSON persistence.
pub struct StatsStore {
    pub value: Stats,
    path: PathBuf,
}

impl StatsStore {
    /// Loads stats alongside settings; missing or corrupt files start empty.
    pub fn load() -> io::Result<Self> {
        Ok(Self::at(config_dir()?.join("stats.json")))
    }

    /// Loads an explicit path, useful for isolated hosts and tests.
    pub fn at(path: PathBuf) -> Self {
        let value = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { value, path }
    }

    /// Counts whitespace-separated words and saves today's bucket, ignoring empty text.
    pub fn record(&mut self, text: &str, spoken: Duration) -> io::Result<()> {
        let words = text.split_whitespace().count() as u64;
        if words == 0 {
            return Ok(());
        }
        let day = self.value.days.entry(today().to_string()).or_default();
        day.words += words;
        day.dictations += 1;
        day.spoken_ms += spoken.as_millis() as u64;
        atomic_write(&self.path, &serde_json::to_vec_pretty(&self.value)?)
    }

    /// Clears persisted counts and speeds; retains the value if saving fails.
    pub fn reset(&mut self) -> io::Result<()> {
        let empty = Stats::default();
        atomic_write(&self.path, &serde_json::to_vec_pretty(&empty)?)?;
        self.value = empty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: whitespace counts as words or a dictation; zero spoken time
    // divides by zero; WPM averages daily rates instead of weighting duration;
    // gaps shift the series or old days leak into it; reset fails to persist;
    // corrupt files prevent startup; saved stats contain transcript text.
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "wispx-stats-{}-{}-{}/stats.json",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn date() -> Date {
        Date::from_calendar_date(2026, time::Month::November, 10).unwrap()
    }

    #[test]
    fn records_only_word_counts_and_duration_and_reset_persists() {
        let path = path();
        let mut store = StatsStore::at(path.clone());
        store.record(" \t\n", Duration::from_secs(5)).unwrap();
        store.record("", Duration::ZERO).unwrap();
        assert!(store.value.days.is_empty());
        assert!(!path.exists());
        store
            .record("one\t two\nthree", Duration::from_secs(2))
            .unwrap();
        let loaded = StatsStore::at(path.clone());
        let summary = loaded.value.summary(today());
        assert_eq!(summary.total_words, 3);
        assert_eq!(summary.total_dictations, 1);
        assert_eq!(summary.average_wpm, 90);
        assert_eq!(summary.words_today, 3);
        let json = fs::read_to_string(&path).unwrap();
        assert!(!json.contains("one"));
        assert_eq!(loaded.value.days.values().next().unwrap().spoken_ms, 2_000);
        store.reset().unwrap();
        assert!(store.value.days.is_empty());
        assert!(StatsStore::at(path.clone()).value.days.is_empty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn wpm_uses_total_spoken_time_including_older_days() {
        let mut stats = Stats::default();
        stats.days.insert(
            date().to_string(),
            Day {
                words: 100,
                dictations: 1,
                spoken_ms: 0,
            },
        );
        assert_eq!(stats.summary(date()).average_wpm, 0);
        stats.days.get_mut(&date().to_string()).unwrap().spoken_ms = 60_000;
        stats.days.insert(
            (date() - time::Duration::days(40)).to_string(),
            Day {
                words: 200,
                dictations: 2,
                spoken_ms: 240_000,
            },
        );
        let summary = stats.summary(date());
        assert_eq!(summary.average_wpm, 60);
        assert_eq!(summary.total_words, 300);
        assert_eq!(summary.total_dictations, 3);
    }

    #[test]
    fn series_zero_fills_and_includes_only_the_last_thirty_days() {
        let mut stats = Stats::default();
        for (age, words) in [(30, 900), (29, 10), (2, 20), (0, 30)] {
            stats.days.insert(
                (date() - time::Duration::days(age)).to_string(),
                Day {
                    words,
                    dictations: 1,
                    spoken_ms: 1_000,
                },
            );
        }
        let summary = stats.summary(date());
        let mut expected = [0; 30];
        expected[0] = 10;
        expected[27] = 20;
        expected[29] = 30;
        assert_eq!(summary.daily, expected);
        assert_eq!(summary.words_today, 30);
        assert_eq!(summary.total_words, 960);
    }

    #[test]
    fn corrupt_file_loads_as_empty() {
        let path = path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{broken").unwrap();
        assert!(StatsStore::at(path.clone()).value.days.is_empty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
