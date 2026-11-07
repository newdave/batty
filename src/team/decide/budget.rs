//! Daily Jev spend, persisted in `telemetry.db` keyed by UTC date.
//!
//! The connection lives on the poll thread and is not `Sync`, so callers
//! record spend when a finished job is taken, never from the worker thread.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Today's UTC date as `YYYY-MM-DD`, the key for [`add_spend`].
pub fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Add `usd` and one call to the total for `date`.
pub fn add_spend(conn: &Connection, date: &str, usd: f64) -> Result<()> {
    conn.execute(
        "INSERT INTO decision_spend (date, usd, calls) VALUES (?1, ?2, 1)
         ON CONFLICT(date) DO UPDATE SET
           usd = usd + excluded.usd,
           calls = calls + 1",
        params![date, usd],
    )
    .context("failed to record decision spend")?;
    Ok(())
}

/// Total USD spent on `date`; zero when nothing was recorded.
pub fn spend_on(conn: &Connection, date: &str) -> Result<f64> {
    let usd = conn
        .query_row(
            "SELECT usd FROM decision_spend WHERE date = ?1",
            params![date],
            |row| row.get::<_, f64>(0),
        )
        .optional()
        .context("failed to read decision spend")?;
    Ok(usd.unwrap_or(0.0))
}

/// Number of paid calls recorded on `date`.
pub fn calls_on(conn: &Connection, date: &str) -> Result<u64> {
    let calls = conn
        .query_row(
            "SELECT calls FROM decision_spend WHERE date = ?1",
            params![date],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("failed to read decision call count")?;
    Ok(calls.unwrap_or(0).max(0) as u64)
}

/// True when the recorded spend for `date` has reached `cap_usd`. Without a
/// database nothing is persisted, so this reports false; the daemon layers an
/// in-memory total on top.
pub fn over_cap(conn: Option<&Connection>, date: &str, cap_usd: f64) -> bool {
    let Some(conn) = conn else {
        return false;
    };
    spend_on(conn, date).is_ok_and(|spent| spent >= cap_usd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::telemetry_db::open_in_memory;

    #[test]
    fn spend_accumulates_per_utc_day() {
        let conn = open_in_memory().unwrap();
        add_spend(&conn, "2026-11-07", 0.01).unwrap();
        add_spend(&conn, "2026-11-07", 0.01).unwrap();
        assert!((spend_on(&conn, "2026-11-07").unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(calls_on(&conn, "2026-11-07").unwrap(), 2);
        assert_eq!(spend_on(&conn, "2026-11-08").unwrap(), 0.0);
        assert_eq!(calls_on(&conn, "2026-11-08").unwrap(), 0);
    }

    #[test]
    fn days_are_independent() {
        let conn = open_in_memory().unwrap();
        add_spend(&conn, "2026-11-07", 1.5).unwrap();
        add_spend(&conn, "2026-11-08", 0.25).unwrap();
        assert_eq!(spend_on(&conn, "2026-11-07").unwrap(), 1.5);
        assert_eq!(spend_on(&conn, "2026-11-08").unwrap(), 0.25);
    }

    #[test]
    fn over_cap_triggers_at_the_cap_and_resets_next_day() {
        let conn = open_in_memory().unwrap();
        assert!(!over_cap(Some(&conn), "2026-11-07", 5.0));
        add_spend(&conn, "2026-11-07", 4.99).unwrap();
        assert!(!over_cap(Some(&conn), "2026-11-07", 5.0));
        add_spend(&conn, "2026-11-07", 0.01).unwrap();
        assert!(over_cap(Some(&conn), "2026-11-07", 5.0));
        assert!(!over_cap(Some(&conn), "2026-11-08", 5.0));
    }

    #[test]
    fn over_cap_without_a_database_is_false() {
        assert!(!over_cap(None, "2026-11-07", 0.0));
    }

    #[test]
    fn zero_cap_blocks_immediately() {
        let conn = open_in_memory().unwrap();
        assert!(over_cap(Some(&conn), "2026-11-07", 0.0));
    }

    #[test]
    fn today_utc_is_a_date_key() {
        let today = today_utc();
        assert_eq!(today.len(), 10);
        assert!(chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d").is_ok());
    }
}
