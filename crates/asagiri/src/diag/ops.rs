//! Operation context shared by the application and its HTTP client.

use std::time::SystemTime;

thread_local! {
    /// The action this thread works for, if any: every line it writes
    /// carries it as `op`, and `client` passes it to the engine.
    static OP: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// A new op id, `run-7c2e`: what the window started, and 4 random hex
/// digits, so one action's lines (window and engine) can be picked out.
pub fn new_op(kind: &str) -> String {
    use std::hash::BuildHasher;
    let kind: String = kind.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
    let n = std::collections::hash_map::RandomState::new().hash_one(SystemTime::now()) as u16;
    format!("{}-{n:04x}", if kind.is_empty() { "op" } else { &kind })
}

/// An op id as it may arrive from elsewhere (a header, `ASAGIRI_OP`).
pub fn valid_op(op: &str) -> bool {
    !op.is_empty() && op.len() <= 40 && op.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// This thread's op.
pub fn op() -> Option<String> {
    OP.with(|o| o.borrow().clone())
}

/// Puts the previous op back when dropped.
#[derive(Debug)]
pub struct OpScope(Option<String>);

impl Drop for OpScope {
    fn drop(&mut self) {
        let prev = self.0.take();
        OP.with(|o| *o.borrow_mut() = prev);
    }
}

/// Works for `op` on this thread until the scope ends.
pub fn enter_op(op: String) -> OpScope {
    OP.with(|o| OpScope(o.borrow_mut().replace(op)))
}

/// The header that carries this thread's op to the engine, or nothing.
pub fn op_header() -> String {
    op().map(|o| format!("X-Asagiri-Op: {o}\r\n")).unwrap_or_default()
}
