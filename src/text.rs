// Extracts full text from PDF attachments by shelling out to `pdftotext`
// (poppler-utils) rather than pulling in a Rust PDF-parsing crate -- see
// DESIGN.md Phase 7. This module is the trust boundary of the project: the
// input is an arbitrary file on disk, and the child process it spawns must
// not be able to hang, blow up memory, or leak its own stderr into ours.

use std::sync::mpsc::{self, Receiver};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TEXT_BYTES: usize = 10 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_STDERR_BYTES: usize = 64 * 1024;

pub fn extract_text(path: &Path) -> Result<String, String> {
    let ext_is_pdf = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false);
    if !ext_is_pdf {
        return Err(format!(
            "cannot extract text from {}: only PDF attachments are supported",
            path.display()
        ));
    }

    // A relative path could be parsed by pdftotext as a flag if it starts
    // with '-'. We use Command (not a shell), so there's no injection vector,
    // but that leading-dash argument confusion is still real. Stored
    // attachment paths are always canonicalized (see resolve_attachment_path),
    // so this should never trip in practice -- it's a guard, not a filter.
    if !path.is_absolute() {
        return Err(format!(
            "cannot extract text from {}: path must be absolute",
            path.display()
        ));
    }

    if !path.exists() {
        return Err(format!("cannot extract text: {} does not exist", path.display()));
    }

    let deadline = Instant::now() + EXTRACT_TIMEOUT;

    let mut cmd = Command::new("pdftotext");
    cmd.args(["-enc", "UTF-8", "-q"])
        .arg(path)
        .arg("-") // write to stdout
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run pdftotext ({e}); is poppler-utils installed?"))?;

    // Drain stdout/stderr on their own threads: pdftotext's output can exceed
    // the pipe buffer (~64KB), and polling try_wait() without draining the
    // pipe would deadlock the child on every large PDF.
    //
    // Channels rather than JoinHandle::join(), because a join can't be given a
    // deadline -- see the drain below for why that matters.
    let stdout_rx = drain(child.stdout.take().expect("stdout was piped"), MAX_TEXT_BYTES + 1);
    let stderr_rx = drain(child.stderr.take().expect("stderr was piped"), MAX_STDERR_BYTES);

    let timed_out = |child: &mut Child, what: &str| {
        kill_process_group(child);
        Err(format!(
            "pdftotext timed out after {}s {} {}",
            EXTRACT_TIMEOUT.as_secs(),
            what,
            path.display()
        ))
    };

    let Some(status) = wait_until(&mut child, deadline) else {
        return timed_out(&mut child, "on");
    };

    // The child exiting does NOT mean the pipe is closed: if pdftotext is a
    // wrapper script (a distro shim, a sandbox launcher), the descendant it
    // forked inherits stdout and holds the write end open. Blocking on the
    // reader here would then hang forever with no timeout in the path at all,
    // so the drain gets the same deadline the wait had.
    let Ok(stdout_bytes) = recv_until(&stdout_rx, deadline) else {
        return timed_out(&mut child, "draining output from");
    };
    // stderr is only ever read for an error message; never let it stall us.
    let stderr_bytes = recv_until(&stderr_rx, deadline).unwrap_or_default();

    if !status.success() {
        let stderr_msg = String::from_utf8_lossy(&stderr_bytes);
        return Err(format!(
            "pdftotext failed on {}: {}",
            path.display(),
            stderr_msg.trim()
        ));
    }

    // pdftotext output isn't guaranteed valid UTF-8 even with -enc UTF-8, so
    // this must never be from_utf8(...).unwrap().
    let text = String::from_utf8_lossy(&stdout_bytes).into_owned();
    Ok(truncate_to_char_boundary(text, MAX_TEXT_BYTES))
}

// Reads a pipe on its own thread, capped at `limit` bytes. The cap is applied
// to the READ, not to the finished string: buffering an unbounded Vec and
// trimming it afterwards would let a PDF that expands to gigabytes of text
// exhaust memory long before any truncation ran.
fn drain<R: std::io::Read + Send + 'static>(mut reader: R, limit: usize) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Past the cap we keep reading but stop keeping. Simply
                    // stopping the read would leave the child blocked on a
                    // full pipe until the timeout, turning a merely enormous
                    // PDF into a 30s failure instead of a truncated success.
                    if buf.len() < limit {
                        let keep = n.min(limit - buf.len());
                        buf.extend_from_slice(&chunk[..keep]);
                    }
                }
            }
        }
        let _ = tx.send(buf);
    });
    rx
}

fn recv_until<T>(rx: &Receiver<T>, deadline: Instant) -> Result<T, ()> {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| ())
}

#[cfg(unix)]
fn own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // Puts the child in its own process group so the whole tree can be killed
    // by pgid; without it, a wrapper script's children survive.
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn own_process_group(_cmd: &mut Command) {}

// Kills the child AND anything it spawned. `child.kill()` signals only the
// immediate process, which leaves descendants running and holding our pipes.
// Shelling out to kill(1) with a negative pid (the process group) avoids
// taking a libc dependency and the first `unsafe` block in this codebase,
// for a tool that already shells out to pdftotext and xdg-open.
fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            // The `--` is load-bearing: util-linux's kill(1) parses a bare
            // negative pid as options, exits 0, and kills nothing.
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn truncate_to_char_boundary(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let cut = text.floor_char_boundary(max_bytes);
    text.truncate(cut);
    text.push_str(&format!(
        "\n\n[...truncated: extracted text exceeded {}MB...]",
        max_bytes / (1024 * 1024)
    ));
    text
}

// std has no process timeout, so poll try_wait() in a loop.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The happy path (a real PDF -> real text) needs a pdftotext binary and a
    // PDF fixture, neither of which belong in a unit test; it's covered by
    // the Phase 7 smoke test instead. This test covers what's checkable
    // without either: the three input-validation guards.
    #[test]
    fn rejects_bad_inputs_before_spawning_pdftotext() {
        assert!(extract_text(Path::new("/tmp/notes.txt")).is_err(), "non-PDF extension");
        assert!(extract_text(Path::new("relative.pdf")).is_err(), "relative path");
        assert!(
            extract_text(Path::new("/definitely/not/a/real/path/x.pdf")).is_err(),
            "missing file"
        );
    }

    #[test]
    fn truncate_to_char_boundary_cuts_at_a_char_boundary_and_names_the_real_limit() {
        // Under the limit: untouched.
        assert_eq!(truncate_to_char_boundary("hello".to_string(), 10), "hello");

        // Over the limit, and the naive byte cut ("héllo" cut at 3 bytes)
        // would land inside the 2-byte 'é' -- must round down instead of
        // panicking or corrupting the string.
        let truncated = truncate_to_char_boundary("héllo".to_string(), 3);
        assert!(truncated.starts_with('h'), "must not slice through a multibyte char");
        assert!(truncated.contains("exceeded 0MB"), "message must reflect the actual limit passed in, not a hardcoded one: {truncated}");
    }
}
