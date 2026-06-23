//! Structured lifecycle event logging for Lecs tasks.

use std::io::Write;

use serde::Serialize;

/// Types of lifecycle events emitted during task execution.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum EventType {
    /// Container image was pulled.
    ImagePulled,
    /// Container was created.
    Created,
    /// Container was started.
    Started,
    /// Container health check passed.
    HealthCheckPassed,
    /// Container health check failed.
    HealthCheckFailed,
    /// Container exited.
    Exited,
    /// Cleanup completed for the task.
    CleanupCompleted,
    /// Container is being restarted (service mode).
    Restarting,
    /// Container exceeded maximum restart count (service mode).
    MaxRestartsExceeded,
}

/// A structured lifecycle event.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleEvent {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Type of event.
    pub event_type: EventType,
    /// Container name (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    /// Task family name.
    pub family: String,
    /// Additional details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl LifecycleEvent {
    /// Create a new lifecycle event with the current timestamp.
    pub fn new(
        event_type: EventType,
        family: &str,
        container_name: Option<&str>,
        details: Option<&str>,
    ) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event_type,
            container_name: container_name.map(String::from),
            family: family.to_string(),
            details: details.map(String::from),
        }
    }
}

/// Trait for receiving lifecycle events.
pub trait EventSink: Send + Sync {
    /// Emit a lifecycle event.
    fn emit(&self, event: &LifecycleEvent);
}

/// Event sink that writes NDJSON to stderr.
pub struct NdjsonEventSink;

impl EventSink for NdjsonEventSink {
    #[allow(clippy::print_stderr)]
    fn emit(&self, event: &LifecycleEvent) {
        if let Ok(json) = serde_json::to_string(event) {
            let _ = writeln!(std::io::stderr(), "{json}");
        }
    }
}

/// Event sink that discards all events (used when `--events` is not specified).
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn emit(&self, _event: &LifecycleEvent) {
        // Intentionally empty
    }
}

/// Event sink that collects events into a vector (for testing).
#[cfg(test)]
pub struct CollectingEventSink {
    events: std::sync::Mutex<Vec<LifecycleEvent>>,
}

#[cfg(test)]
impl CollectingEventSink {
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    #[allow(clippy::unwrap_used)]
    pub fn events(&self) -> Vec<LifecycleEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl EventSink for CollectingEventSink {
    fn emit(&self, event: &LifecycleEvent) {
        #[allow(clippy::unwrap_used)]
        self.events.lock().unwrap().push(event.clone());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_event_serializes_to_json() {
        let event = LifecycleEvent {
            timestamp: "2025-01-15T10:00:00Z".to_string(),
            event_type: EventType::Started,
            container_name: Some("web".to_string()),
            family: "my-app".to_string(),
            details: None,
        };
        let json = serde_json::to_string(&event).expect("should serialize");
        assert!(json.contains("\"event_type\":\"started\""));
        assert!(json.contains("\"container_name\":\"web\""));
        assert!(json.contains("\"family\":\"my-app\""));
        assert!(!json.contains("details")); // None is skipped
    }

    #[test]
    fn lifecycle_event_with_details() {
        let event = LifecycleEvent {
            timestamp: "2025-01-15T10:00:00Z".to_string(),
            event_type: EventType::Exited,
            container_name: Some("web".to_string()),
            family: "my-app".to_string(),
            details: Some("exit code: 0".to_string()),
        };
        let json = serde_json::to_string(&event).expect("should serialize");
        assert!(json.contains("\"details\":\"exit code: 0\""));
    }

    #[test]
    fn null_event_sink_does_nothing() {
        let sink = NullEventSink;
        let event = LifecycleEvent::new(EventType::Started, "test", Some("web"), None);
        sink.emit(&event); // Should not panic
    }

    #[test]
    fn collecting_event_sink_captures_events() {
        let sink = CollectingEventSink::new();

        sink.emit(&LifecycleEvent::new(
            EventType::Created,
            "my-app",
            Some("web"),
            None,
        ));
        sink.emit(&LifecycleEvent::new(
            EventType::Started,
            "my-app",
            Some("web"),
            None,
        ));

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event_type, EventType::Created));
        assert!(matches!(events[1].event_type, EventType::Started));
    }

    #[test]
    fn event_type_serialization() {
        let types = vec![
            (EventType::Created, "\"created\""),
            (EventType::Started, "\"started\""),
            (EventType::HealthCheckPassed, "\"health_check_passed\""),
            (EventType::HealthCheckFailed, "\"health_check_failed\""),
            (EventType::Exited, "\"exited\""),
            (EventType::CleanupCompleted, "\"cleanup_completed\""),
            (EventType::Restarting, "\"restarting\""),
            (EventType::MaxRestartsExceeded, "\"max_restarts_exceeded\""),
        ];

        for (event_type, expected) in types {
            let json = serde_json::to_string(&event_type).expect("should serialize");
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn lifecycle_event_new_sets_timestamp() {
        let event = LifecycleEvent::new(EventType::Started, "my-app", Some("web"), None);
        assert!(!event.timestamp.is_empty());
        assert_eq!(event.family, "my-app");
        assert_eq!(event.container_name.as_deref(), Some("web"));
    }

    #[test]
    fn ndjson_event_sink_emits_without_panic() {
        let sink = NdjsonEventSink;
        let event = LifecycleEvent::new(EventType::Created, "test-app", Some("web"), Some("test"));
        sink.emit(&event); // Should write to stderr without panic
    }
}
