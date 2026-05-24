//! Capture FD 2 into an in-memory buffer for the lifetime of the TUI, then replay it to the
//! real stderr on drop. This lets anything that writes to stderr (tracing logs, library
//! warnings like arboard's, panic messages) stay invisible while the alt-screen is active and
//! show up in the user's shell after the editor exits.

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::{Arc, Mutex};

pub struct StderrCapture {
    saved_stderr_fd: libc::c_int,
    buffer: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl StderrCapture {
    pub fn install() -> std::io::Result<Self> {
        // Save the current FD 2 so we can restore it later.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Create a pipe and point FD 2 at its write end. Anything writing to stderr now flows
        // into the pipe; we drain the read end on a background thread.
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(saved) };
            return Err(e);
        }
        let read_fd = fds[0];
        let write_fd = fds[1];
        if unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::close(saved);
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(e);
        }
        // FD 2 now holds the only reference to the pipe's write end the world uses; drop our
        // local `write_fd` int so closing FD 2 (in `Drop`) is what signals EOF to the reader.
        unsafe { libc::close(write_fd) };

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let buffer_thread = buffer.clone();
        let reader = std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let mut buf = [0u8; 4096];
            loop {
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut g) = buffer_thread.lock() {
                            g.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            }
        });

        Ok(Self {
            saved_stderr_fd: saved,
            buffer,
            reader: Some(reader),
        })
    }
}

impl Drop for StderrCapture {
    fn drop(&mut self) {
        // Restore the real stderr first; this also drops the last reference to the pipe's
        // write end so the reader thread sees EOF.
        unsafe {
            libc::dup2(self.saved_stderr_fd, libc::STDERR_FILENO);
            libc::close(self.saved_stderr_fd);
        }
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        if let Ok(buf) = self.buffer.lock() {
            if !buf.is_empty() {
                let _ = std::io::stderr().write_all(&buf);
                let _ = std::io::stderr().flush();
            }
        }
    }
}
