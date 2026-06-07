//! System clipboard access — the browser analog of the TUI's arboard. Aether's clipboard is the OS
//! clipboard, mediated by the client: copy/cut get the text from the server and write it here; paste
//! reads it and feeds it back through input/text. Called within a keydown user gesture on a secure
//! context (localhost), which is what the async Clipboard API requires; readText may prompt for
//! permission the first time. Callers handle rejection (e.g. a denied permission) with a status.

export function writeClipboard(text: string): Promise<void> {
  return navigator.clipboard.writeText(text);
}

export function readClipboard(): Promise<string> {
  return navigator.clipboard.readText();
}
