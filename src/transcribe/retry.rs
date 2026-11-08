//! Bounded transport retry policy; authentication refresh has its own budget.

use std::time::{Duration, SystemTime};

/// `retry` is zero before the second attempt. `None` status means network failure.
pub(super) fn delay(
    retry: u32,
    status: Option<u16>,
    retry_after: Option<&str>,
    now: SystemTime,
) -> Option<Duration> {
    if retry >= 2 || !matches!(status, None | Some(408 | 500 | 502 | 503 | 504)) {
        return None;
    }
    let server_wait = retry_after
        .and_then(|header| {
            header
                .trim()
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    httpdate::parse_http_date(header)
                        .ok()
                        .map(|date| date.duration_since(now).unwrap_or_default())
                })
        })
        .unwrap_or_default();
    let wait = Duration::from_millis(500 << retry).max(server_wait);
    (wait <= Duration::from_secs(10)).then_some(wait)
}

#[cfg(test)]
mod tests {
    // Failure cases: permanent statuses retry; a fourth attempt slips through;
    // backoff or Retry-After is ignored; date parsing, stale dates, overflow, or
    // invalid headers bypass the ten-second wait cap.
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn retries_only_network_and_the_five_transient_statuses_twice() {
        for status in [None, Some(408), Some(500), Some(502), Some(503), Some(504)] {
            assert_eq!(
                delay(0, status, None, UNIX_EPOCH),
                Some(Duration::from_millis(500))
            );
            assert_eq!(
                delay(1, status, None, UNIX_EPOCH),
                Some(Duration::from_secs(1))
            );
            assert_eq!(delay(2, status, None, UNIX_EPOCH), None);
        }
        for status in [200, 400, 401, 403, 413, 429, 501] {
            assert_eq!(delay(0, Some(status), Some("2"), UNIX_EPOCH), None);
        }
    }

    #[test]
    fn respects_retry_after_seconds_and_dates_without_waiting_over_ten_seconds() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for retry in 0..2 {
            for (header, expected) in [
                ("0", Some(500_u64 << retry)),
                ("2", Some(2000)),
                ("10", Some(10000)),
                ("11", None),
                ("18446744073709551615", None),
                ("garbage", Some(500_u64 << retry)),
                ("-1", Some(500_u64 << retry)),
            ] {
                assert_eq!(
                    delay(retry, Some(503), Some(header), now),
                    expected.map(Duration::from_millis)
                );
            }
            for (offset, expected) in [(3, Some(3000)), (10, Some(10000)), (11, None)] {
                let date = httpdate::fmt_http_date(now + Duration::from_secs(offset));
                assert_eq!(
                    delay(retry, Some(503), Some(&date), now),
                    expected.map(Duration::from_millis)
                );
            }
            let past = httpdate::fmt_http_date(now - Duration::from_secs(60));
            assert_eq!(
                delay(retry, Some(503), Some(&past), now),
                Some(Duration::from_millis(500 << retry))
            );
        }
    }
}
