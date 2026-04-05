//! Composable `ToolExecutor` wrappers for crosscutting concerns.
//!
//! Each wrapper implements `ToolExecutor` by delegating to an inner executor
//! after applying a concern (permission checks, rate limiting, audit logging).
//!
//! Composition example:
//! ```text
//! AuditLoggingExecutor(RateLimitedExecutor(inner))
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::conversation::{ToolError, ToolExecutor};
use crate::PermissionMode;

// ── Rate Limiting ───────────────────────────────────────────────────

/// Sliding-window rate limiter per tool name.
///
/// Wraps an inner `ToolExecutor` and rejects calls when a tool exceeds
/// `max_calls` within `window`.
pub struct RateLimitedExecutor<T> {
    inner: T,
    max_calls: usize,
    window: Duration,
    history: BTreeMap<String, Vec<Instant>>,
}

impl<T: ToolExecutor> RateLimitedExecutor<T> {
    /// Create a new rate-limited wrapper.
    ///
    /// `max_calls` is the maximum number of invocations allowed per `window`
    /// for each distinct tool name.
    pub fn new(inner: T, max_calls: usize, window: Duration) -> Self {
        Self {
            inner,
            max_calls,
            window,
            history: BTreeMap::new(),
        }
    }
}

impl<T: ToolExecutor> ToolExecutor for RateLimitedExecutor<T> {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let now = Instant::now();
        let timestamps = self.history.entry(tool_name.to_string()).or_default();

        // Prune expired entries
        timestamps.retain(|&ts| now.duration_since(ts) < self.window);

        if timestamps.len() >= self.max_calls {
            return Err(ToolError::new(format!(
                "rate limit exceeded for tool `{tool_name}`: max {max} calls per {window}s",
                max = self.max_calls,
                window = self.window.as_secs(),
            )));
        }

        timestamps.push(now);
        self.inner.execute(tool_name, input)
    }
}

// ── Permission Guard ────────────────────────────────────────────────

/// Guards tool execution based on required permission levels.
///
/// Each tool is mapped to a required `PermissionMode`. If the executor's
/// active mode is insufficient, the call is rejected without reaching
/// the inner executor.
pub struct PermissionGuardedExecutor<T> {
    inner: T,
    active_mode: PermissionMode,
    tool_requirements: BTreeMap<String, PermissionMode>,
}

impl<T: ToolExecutor> PermissionGuardedExecutor<T> {
    /// Create a new permission-guarded wrapper.
    ///
    /// `active_mode` is the current session's permission level.
    /// `tool_requirements` maps tool names to their minimum required mode.
    pub fn new(
        inner: T,
        active_mode: PermissionMode,
        tool_requirements: BTreeMap<String, PermissionMode>,
    ) -> Self {
        Self {
            inner,
            active_mode,
            tool_requirements,
        }
    }
}

impl<T: ToolExecutor> ToolExecutor for PermissionGuardedExecutor<T> {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if let Some(&required) = self.tool_requirements.get(tool_name) {
            if self.active_mode < required {
                return Err(ToolError::new(format!(
                    "permission denied for tool `{tool_name}`: requires {required:?}, active mode is {:?}",
                    self.active_mode,
                )));
            }
        }
        self.inner.execute(tool_name, input)
    }
}

// ── Audit Logging ───────────────────────────────────────────────────

/// Records a timestamped audit entry for every tool execution.
///
/// Entries include tool name, success/failure, and wall-clock duration.
/// The log is stored behind an `Arc<Mutex<_>>` so callers can inspect
/// it after the session ends.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub tool_name: String,
    pub success: bool,
    pub duration: Duration,
    pub error_message: Option<String>,
}

pub struct AuditLoggingExecutor<T> {
    inner: T,
    log: Arc<Mutex<Vec<AuditEntry>>>,
}

impl<T: ToolExecutor> AuditLoggingExecutor<T> {
    /// Create a new audit-logging wrapper.
    ///
    /// The returned `Arc` handle can be cloned and used to read the audit
    /// log after the session completes.
    pub fn new(inner: T) -> (Self, Arc<Mutex<Vec<AuditEntry>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner,
                log: Arc::clone(&log),
            },
            log,
        )
    }
}

impl<T: ToolExecutor> ToolExecutor for AuditLoggingExecutor<T> {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let start = Instant::now();
        let result = self.inner.execute(tool_name, input);
        let duration = start.elapsed();

        let entry = AuditEntry {
            tool_name: tool_name.to_string(),
            success: result.is_ok(),
            duration,
            error_message: result.as_ref().err().map(ToString::to_string),
        };

        if let Ok(mut log) = self.log.lock() {
            log.push(entry);
        }

        result
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoExecutor;

    impl ToolExecutor for EchoExecutor {
        fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
            Ok(format!("{tool_name}:{input}"))
        }
    }

    struct FailExecutor;

    impl ToolExecutor for FailExecutor {
        fn execute(&mut self, _tool_name: &str, _input: &str) -> Result<String, ToolError> {
            Err(ToolError::new("intentional failure".to_string()))
        }
    }

    // ── Rate limiting tests ─────────────────────────────────────

    #[test]
    fn rate_limited_allows_within_budget() {
        let mut executor =
            RateLimitedExecutor::new(EchoExecutor, 3, Duration::from_secs(60));
        assert!(executor.execute("bash", "{}").is_ok());
        assert!(executor.execute("bash", "{}").is_ok());
        assert!(executor.execute("bash", "{}").is_ok());
    }

    #[test]
    fn rate_limited_rejects_over_budget() {
        let mut executor =
            RateLimitedExecutor::new(EchoExecutor, 2, Duration::from_secs(60));
        assert!(executor.execute("bash", "{}").is_ok());
        assert!(executor.execute("bash", "{}").is_ok());
        let result = executor.execute("bash", "{}");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("rate limit exceeded"),
        );
    }

    #[test]
    fn rate_limited_per_tool_isolation() {
        let mut executor =
            RateLimitedExecutor::new(EchoExecutor, 1, Duration::from_secs(60));
        assert!(executor.execute("bash", "{}").is_ok());
        assert!(executor.execute("read_file", "{}").is_ok());
        // bash is now over limit
        assert!(executor.execute("bash", "{}").is_err());
    }

    // ── Permission guard tests ──────────────────────────────────

    #[test]
    fn permission_allows_sufficient_mode() {
        let mut reqs = BTreeMap::new();
        reqs.insert("bash".to_string(), PermissionMode::WorkspaceWrite);
        let mut executor = PermissionGuardedExecutor::new(
            EchoExecutor,
            PermissionMode::DangerFullAccess,
            reqs,
        );
        assert!(executor.execute("bash", "{}").is_ok());
    }

    #[test]
    fn permission_denies_insufficient_mode() {
        let mut reqs = BTreeMap::new();
        reqs.insert("bash".to_string(), PermissionMode::DangerFullAccess);
        let mut executor = PermissionGuardedExecutor::new(
            EchoExecutor,
            PermissionMode::ReadOnly,
            reqs,
        );
        let result = executor.execute("bash", "{}");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("permission denied"));
    }

    #[test]
    fn permission_allows_unregistered_tool() {
        let mut executor = PermissionGuardedExecutor::new(
            EchoExecutor,
            PermissionMode::ReadOnly,
            BTreeMap::new(),
        );
        assert!(executor.execute("unknown_tool", "{}").is_ok());
    }

    // ── Audit logging tests ─────────────────────────────────────

    #[test]
    fn audit_records_success() {
        let (mut executor, log) = AuditLoggingExecutor::new(EchoExecutor);
        let _ = executor.execute("bash", "{}");
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].success);
        assert_eq!(entries[0].tool_name, "bash");
        assert!(entries[0].error_message.is_none());
    }

    #[test]
    fn audit_records_failure() {
        let (mut executor, log) = AuditLoggingExecutor::new(FailExecutor);
        let _ = executor.execute("bash", "{}");
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].success);
        assert!(entries[0].error_message.as_ref().unwrap().contains("intentional failure"));
    }

    #[test]
    fn audit_records_duration() {
        let (mut executor, log) = AuditLoggingExecutor::new(EchoExecutor);
        let _ = executor.execute("bash", "{}");
        let entries = log.lock().unwrap();
        assert!(entries[0].duration.as_nanos() > 0);
    }

    // ── Composition test ────────────────────────────────────────

    #[test]
    fn composed_rate_limit_then_permission() {
        let mut reqs = BTreeMap::new();
        reqs.insert("bash".to_string(), PermissionMode::WorkspaceWrite);

        let inner = PermissionGuardedExecutor::new(
            EchoExecutor,
            PermissionMode::DangerFullAccess,
            reqs,
        );
        let mut executor = RateLimitedExecutor::new(inner, 10, Duration::from_secs(60));

        assert!(executor.execute("bash", "{}").is_ok());
    }
}
