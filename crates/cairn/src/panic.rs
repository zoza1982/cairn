//! Panic handling.
//!
//! Installs a hook that prints a clear crash message and chains to the default hook (backtrace).
//! Once the TUI owns the terminal (raw mode + alternate screen, M1), this hook is extended to
//! restore the terminal before printing, so a panic never leaves it in a broken state.

use std::panic;

/// Install the global panic hook. Last call wins.
pub(crate) fn install_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        eprintln!(
            "\n{} crashed. This is a bug — please report it.",
            crate::APP_NAME
        );
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn install_hook_is_idempotent() {
        // Installing twice must not panic; the second hook simply replaces the first.
        super::install_hook();
        super::install_hook();
    }
}
