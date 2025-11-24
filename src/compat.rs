/// Compatibility types to match openworkers-runtime API
use std::collections::HashMap;

/// Script with code and environment variables
#[derive(Debug, Clone)]
pub struct Script {
    pub code: String,
    pub env: Option<HashMap<String, String>>,
}

impl Script {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            env: None,
        }
    }

    pub fn with_env(code: impl Into<String>, env: HashMap<String, String>) -> Self {
        Self {
            code: code.into(),
            env: Some(env),
        }
    }
}

/// Runtime resource limits configuration
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    /// Initial V8 heap size in MB (default: 1MB)
    /// Note: JSCore doesn't have explicit heap sizing, this is ignored
    pub heap_initial_mb: usize,
    /// Maximum V8 heap size in MB (default: 128MB)
    /// Note: JSCore doesn't have explicit heap limits, this is ignored
    pub heap_max_mb: usize,
    /// Maximum CPU time in milliseconds (default: 50ms, 0 = disabled)
    /// Note: Not yet implemented in JSCore runtime
    pub max_cpu_time_ms: u64,
    /// Maximum wall-clock time in milliseconds (default: 30s, 0 = disabled)
    /// Note: Not yet implemented in JSCore runtime
    pub max_wall_clock_time_ms: u64,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            heap_initial_mb: 1,
            heap_max_mb: 128,
            max_cpu_time_ms: 50,
            max_wall_clock_time_ms: 30_000,
        }
    }
}

/// Termination reason for worker execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationReason {
    /// Task completed successfully
    Success,
    /// Uncaught exception in JavaScript
    Exception,
    /// CPU time limit exceeded (not yet implemented)
    CpuTimeLimit,
    /// Wall clock timeout exceeded (not yet implemented)
    WallClockTimeout,
    /// Memory limit exceeded (not yet implemented)
    MemoryLimit,
    /// Worker failed to initialize
    InitializationError,
    /// Worker was terminated externally (not yet implemented)
    Terminated,
}

impl TerminationReason {
    /// Get HTTP status code for this termination reason
    pub fn http_status(&self) -> u16 {
        match self {
            TerminationReason::Success => 200,
            TerminationReason::Exception => 500,
            TerminationReason::CpuTimeLimit => 503,
            TerminationReason::WallClockTimeout => 504,
            TerminationReason::MemoryLimit => 507,
            TerminationReason::InitializationError => 500,
            TerminationReason::Terminated => 503,
        }
    }
}

/// Log event from JavaScript console
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Log,
    Debug,
    Trace,
}

impl LogLevel {
    /// Parse log level from string
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "error" => LogLevel::Error,
            "warn" => LogLevel::Warn,
            "info" => LogLevel::Info,
            "log" => LogLevel::Log,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => LogLevel::Info, // Default to Info for unknown levels
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Error => write!(f, "ERROR"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Log => write!(f, "LOG"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Trace => write!(f, "TRACE"),
        }
    }
}
