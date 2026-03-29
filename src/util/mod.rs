pub mod digest;
pub mod fetch;

/// Emit a bracketed progress line to stderr.
///
/// ```text
/// progress!("build", "packing {} layer(s)", n);
/// // stderr: [build] packing 3 layer(s)
/// ```
///
/// Each command module defines a thin local alias so the prefix is never
/// repeated at every call site:
///
/// ```rust,ignore
/// macro_rules! log { ($($t:tt)*) => { crate::progress!("build", $($t)*) } }
/// log!("packing {} layer(s)", n);
/// ```
#[macro_export]
macro_rules! progress {
    ($prefix:expr, $($arg:tt)*) => {
        eprintln!("[{}] {}", $prefix, format_args!($($arg)*))
    };
}
