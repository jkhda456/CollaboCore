//! Errors as git-lfs builds them (errors/types.go): a message chain (`outer: inner`, as
//! pkg/errors.Wrap prints) and the kinds callers test for.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Fatal,
    NotAPointer,
    BadPointerKey,
    Auth,
    Smudge,
    NotImplemented,
    Retriable,
    RetriableLater,
    DownloadDeclined,
    Unprocessable,
    Protocol,
    PointerScan,
    NotFound,
}

#[derive(Clone, Debug)]
pub struct Error {
    pub kind: Kind,
    pub msg: String,
    pub cause: Option<Box<Error>>,
    /// For retriable-later errors: when to try again (Retry-After).
    pub retry_at: Option<std::time::SystemTime>,
    /// A bad pointer key error's expected key.
    pub expected: Option<String>,
    /// The exit status of a failed command.
    pub exit_status: Option<i32>,
    /// The HTTP status of the response an error came from.
    pub http_status: Option<u32>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Error {
        Error { kind: Kind::Plain, msg: msg.into(), cause: None, retry_at: None, expected: None, exit_status: None, http_status: None }
    }
    /// pkg/errors.Wrap: `msg: cause`.
    pub fn wrap(self, msg: impl Into<String>) -> Error {
        let kind = self.kind;
        Error { kind, msg: msg.into(), cause: Some(Box::new(self)), retry_at: None, expected: None, exit_status: None, http_status: None }
    }
    pub fn with_kind(mut self, kind: Kind) -> Error {
        self.kind = kind;
        self
    }
    pub fn not_a_pointer(cause: Error) -> Error {
        cause.wrap("Pointer file error").with_kind(Kind::NotAPointer)
    }
    pub fn is(&self, k: Kind) -> bool {
        let mut e = Some(self);
        while let Some(x) = e {
            if x.kind == k {
                return true;
            }
            e = x.cause.as_deref();
        }
        false
    }
    /// errors.newWrappedError: `msg: self` with a message; without one, `self` as it is if it
    /// already wraps something, else `LFS: self`; under the kind given.
    pub fn go_wrap(self, kind: Kind, msg: &str) -> Error {
        let base = if !msg.is_empty() {
            self.wrap(msg)
        } else if self.cause.is_some() {
            self
        } else {
            self.wrap("LFS")
        };
        Error { kind, msg: String::new(), cause: Some(Box::new(base)), retry_at: None, expected: None, exit_status: None, http_status: None }
    }
    pub fn auth(self) -> Error {
        self.go_wrap(Kind::Auth, "Authentication required")
    }
    pub fn go_fatal(self) -> Error {
        self.go_wrap(Kind::Fatal, "Fatal error")
    }
    pub fn retriable(self) -> Error {
        self.go_wrap(Kind::Retriable, "")
    }
    pub fn unprocessable(self) -> Error {
        self.go_wrap(Kind::Unprocessable, "")
    }
    pub fn protocol(msg: &str, cause: Option<Error>) -> Error {
        cause.unwrap_or_else(|| Error::new("Error")).go_wrap(Kind::Protocol, msg)
    }
    /// errors.NewRetriableLaterError: None when the Retry-After header gives no time (seconds,
    /// or an RFC 1123 date).
    pub fn retriable_later(self, header: &str) -> Option<Error> {
        if header.is_empty() {
            return None;
        }
        let at = if let Ok(secs) = header.trim().parse::<i64>() {
            let now = std::time::SystemTime::now();
            if secs >= 0 {
                now + std::time::Duration::from_secs(secs as u64)
            } else {
                now - std::time::Duration::from_secs((-secs) as u64)
            }
        } else {
            crate::tools::parse_rfc1123(header)?
        };
        let mut e = self.go_wrap(Kind::RetriableLater, "");
        e.retry_at = Some(at);
        Some(e)
    }
    /// IsRetriableLaterError: the time from the outermost retriable-later error.
    pub fn retry_later_at(&self) -> Option<std::time::SystemTime> {
        let mut e = Some(self);
        while let Some(x) = e {
            if x.retry_at.is_some() {
                return x.retry_at;
            }
            e = x.cause.as_deref();
        }
        None
    }
    pub fn innermost(&self) -> &Error {
        let mut e = self;
        while let Some(c) = e.cause.as_deref() {
            e = c;
        }
        e
    }
    pub fn exit_status(&self) -> Option<i32> {
        let mut e = Some(self);
        while let Some(x) = e {
            if x.exit_status.is_some() {
                return x.exit_status;
            }
            e = x.cause.as_deref();
        }
        None
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match (&self.cause, self.msg.is_empty()) {
            (Some(c), true) => write!(f, "{c}"),
            (Some(c), false) => write!(f, "{}: {c}", self.msg),
            (None, _) => f.write_str(&self.msg),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(crate::tools::io_err(&e))
    }
}

#[macro_export]
macro_rules! bail {
    ($($a:tt)*) => { return Err($crate::errors::Error::new(format!($($a)*))) };
}
