//! Clipboard helpers. We keep a single `arboard::Clipboard` handle alive for the app's lifetime;
//! creating one per operation generates a noisy "clipboard dropped too quickly" warning on
//! Linux because X11 clipboard ownership is tied to a live process.

use arboard::Clipboard;

pub fn new_handle() -> Option<Clipboard> {
    Clipboard::new().ok()
}

pub fn copy(handle: &mut Option<Clipboard>, text: String) -> anyhow::Result<()> {
    match handle.as_mut() {
        Some(c) => Ok(c.set_text(text)?),
        None => anyhow::bail!("clipboard unavailable"),
    }
}

pub fn paste(handle: &mut Option<Clipboard>) -> anyhow::Result<String> {
    match handle.as_mut() {
        Some(c) => Ok(c.get_text()?),
        None => anyhow::bail!("clipboard unavailable"),
    }
}
