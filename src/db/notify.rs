//! Notify endpoint registry helpers.
//!
//! Owns the `notify_endpoints` table — `(instance, kind, port)` rows that map
//! an instance to TCP ports the rest of the system can poke. Two protocol
//! families share this table:
//!
//! - **Wake endpoints** (kinds: `pty`, `hook`, `listen`, `listen_filter`,
//!   `events_wait`, `plugin`) — connect-and-close wakes a poll loop in the
//!   target process. See `crate::notify::WakeKind`.
//! - **Inject endpoint** (kind: `inject`) — bidirectional RPC for PTY input
//!   and screen queries. Lives in the same table for historical reasons; the
//!   protocol is unrelated to wake.

use anyhow::Result;
use rusqlite::params;

use super::HcomDb;
use crate::shared::time::now_epoch_f64;

impl HcomDb {
    /// Register notify endpoint for PTY wake-ups
    ///
    /// Inserts or updates notify_endpoints table with (instance, kind='pty', port)
    pub fn register_notify_port(&self, name: &str, port: u16) -> Result<()> {
        self.upsert_notify_endpoint(name, "pty", port)
    }

    /// Register inject port for screen queries
    pub fn register_inject_port(&self, name: &str, port: u16) -> Result<()> {
        self.upsert_notify_endpoint(name, "inject", port)
    }

    /// Delete notify endpoints for an instance
    pub fn delete_notify_endpoints(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?",
            params![name],
        )?;
        Ok(())
    }

    /// Insert or update a notify endpoint with specific kind.
    /// Used by listen command to register listen/listen_filter endpoints.
    pub fn upsert_notify_endpoint(&self, name: &str, kind: &str, port: u16) -> Result<()> {
        let now = now_epoch_f64();

        self.conn.execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(instance, kind) DO UPDATE SET
                 port = excluded.port,
                 updated_at = excluded.updated_at",
            params![name, kind, port as i64, now],
        )?;
        Ok(())
    }

    /// Re-register a PTY's `pty` + `inject` endpoints only while its instance
    /// row exists, so a delivery loop waking on its own stop can't recreate them.
    pub fn refresh_pty_endpoints(
        &self,
        name: &str,
        notify_port: u16,
        inject_port: u16,
    ) -> Result<()> {
        let now = now_epoch_f64();
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM instances WHERE name = ?1)
             ON CONFLICT(instance, kind) DO UPDATE SET
                 port = excluded.port,
                 updated_at = excluded.updated_at",
        )?;
        stmt.execute(params![name, "pty", notify_port as i64, now])?;
        stmt.execute(params![name, "inject", inject_port as i64, now])?;
        Ok(())
    }

    /// Delete PTY endpoints of missing instances not refreshed within `grace_secs`.
    pub fn prune_orphan_endpoints(&self, grace_secs: f64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM notify_endpoints
             WHERE kind IN ('pty', 'inject')
               AND updated_at < ?
               AND instance NOT IN (SELECT name FROM instances)",
            params![now_epoch_f64() - grace_secs],
        )?)
    }

    /// Delete a specific notify endpoint by instance and kind.
    pub fn delete_notify_endpoint(&self, name: &str, kind: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ? AND kind = ?",
            params![name, kind],
        )?;
        Ok(())
    }

    /// Check if any notify endpoint exists for an instance.
    pub fn has_notify_endpoint(&self, name: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM notify_endpoints WHERE instance = ? LIMIT 1",
                params![name],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Check whether a notify endpoint of a specific `kind` exists for an
    /// instance. `kind='plugin'` is registered only when a plugin-driven tool
    /// (pi/omp) has bound its extension — a rendering-independent proof that the
    /// interactive TUI is up, used as a launch-readiness signal where the
    /// on-screen ready pattern is theme/preset dependent.
    pub fn has_notify_endpoint_kind(&self, name: &str, kind: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM notify_endpoints WHERE instance = ? AND kind = ? LIMIT 1",
                params![name, kind],
                |_| Ok(()),
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{cleanup_test_db, setup_full_test_db};

    #[test]
    fn test_register_inject_port_inserts() {
        let (db, db_path) = setup_full_test_db();

        db.register_inject_port("test", 5555).unwrap();

        let port: i64 = db
            .conn
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(port, 5555);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_register_inject_port_upserts() {
        let (db, db_path) = setup_full_test_db();

        db.register_inject_port("test", 5555).unwrap();
        db.register_inject_port("test", 6666).unwrap();

        let port: i64 = db
            .conn
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(port, 6666);

        // Should be exactly one row
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance = 'test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_refresh_pty_endpoints_skips_missing_instance_and_prune_clears_orphans() {
        let (db, db_path) = setup_full_test_db();
        let count = |name: &str| -> i64 {
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM notify_endpoints WHERE instance = ?",
                    [name],
                    |r| r.get(0),
                )
                .unwrap()
        };

        // A stopped instance's delivery loop must not resurrect its endpoints.
        db.refresh_pty_endpoints("gone", 5555, 5556).unwrap();
        assert_eq!(count("gone"), 0);

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('live', 1000.0)",
                [],
            )
            .unwrap();
        db.refresh_pty_endpoints("live", 6665, 6666).unwrap();
        assert_eq!(count("live"), 2);

        // Pre-existing orphans: pruned only once past the grace window, and
        // only PTY kinds.
        db.register_notify_port("orphan", 7777).unwrap();
        db.upsert_notify_endpoint("orphan", "listen", 7778).unwrap();
        assert_eq!(db.prune_orphan_endpoints(60.0).unwrap(), 0);
        assert_eq!(db.prune_orphan_endpoints(-1.0).unwrap(), 1);
        assert_eq!(count("orphan"), 1);
        assert_eq!(count("live"), 2);

        cleanup_test_db(db_path);
    }
}
