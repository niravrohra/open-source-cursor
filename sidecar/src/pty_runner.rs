use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

static NEXT_PROC_ID: AtomicU64 = AtomicU64::new(1);

const MAX_BG_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_PTY_ROWS: u16 = 24;
const DEFAULT_PTY_COLS: u16 = 120;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundProcessInfo {
    pub proc_id: String,
    pub command: String,
    pub cwd: String,
    pub started_at: u64,
    pub running: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOutput {
    pub proc_id: String,
    pub command: String,
    pub output: String,
    pub running: bool,
    pub exit_code: Option<i32>,
}

struct ProcessState {
    command: String,
    cwd: String,
    started_at: u64,
    running: bool,
    exit_code: Option<i32>,
    unread_bytes: Vec<u8>,
}

struct BackgroundProcessHandle {
    state: Arc<Mutex<ProcessState>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    _master: Mutex<Box<dyn MasterPty + Send>>,
}

/// Manages long-running background processes inside real PTYs (`portable-pty`).
#[derive(Clone, Default)]
pub struct PtyManager {
    processes: Arc<Mutex<HashMap<String, Arc<BackgroundProcessHandle>>>>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts `command` in `/bin/zsh -lc` inside a new PTY in `cwd` and tracks it as a background process.
    pub fn start_background(
        &self,
        command: &str,
        cwd: &Path,
    ) -> Result<BackgroundProcessInfo, String> {
        if !cwd.exists() {
            return Err(format!("Working directory does not exist: {}", cwd.display()));
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: DEFAULT_PTY_ROWS,
                cols: DEFAULT_PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to open PTY: {e}"))?;

        let cmd_builder = build_zsh_command(command, cwd);
        let mut child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| format!("Failed to spawn PTY background command: {e}"))?;
        // Drop slave immediately so master reader observes EOF when child exits.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("Failed to take PTY writer: {e}"))?;
        let killer = child.clone_killer();

        let id_num = NEXT_PROC_ID.fetch_add(1, Ordering::Relaxed);
        let proc_id = format!("pty_{id_num}");
        let started_at = epoch_secs();
        let cwd_str = cwd.to_string_lossy().to_string();

        let state = Arc::new(Mutex::new(ProcessState {
            command: command.to_string(),
            cwd: cwd_str.clone(),
            started_at,
            running: true,
            exit_code: None,
            unread_bytes: Vec::new(),
        }));

        let reader_state = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut guard) = reader_state.lock() {
                            guard.unread_bytes.extend_from_slice(&buf[..n]);
                            if guard.unread_bytes.len() > MAX_BG_BUFFER_BYTES {
                                let overflow = guard.unread_bytes.len() - MAX_BG_BUFFER_BYTES;
                                guard.unread_bytes.drain(..overflow);
                            }
                        }
                    }
                    Err(e) if is_pty_eof(&e) => break,
                    Err(_) => break,
                }
            }
        });

        let waiter_state = Arc::clone(&state);
        std::thread::spawn(move || {
            let status = child.wait();
            if let Ok(mut guard) = waiter_state.lock() {
                guard.running = false;
                guard.exit_code = status.ok().map(|s| s.exit_code() as i32);
            }
        });

        let handle = Arc::new(BackgroundProcessHandle {
            state,
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            _master: Mutex::new(pair.master),
        });

        self.processes
            .lock()
            .map_err(|_| "PTY manager lock poisoned".to_string())?
            .insert(proc_id.clone(), handle);

        Ok(BackgroundProcessInfo {
            proc_id,
            command: command.to_string(),
            cwd: cwd_str,
            started_at,
            running: true,
            exit_code: None,
        })
    }

    /// Reads and drains any newly buffered PTY output for `proc_id`.
    pub fn read_output(&self, proc_id: &str) -> Result<ProcessOutput, String> {
        let handle = self.get_handle(proc_id)?;
        let mut guard = handle
            .state
            .lock()
            .map_err(|_| "Process state lock poisoned".to_string())?;
        let bytes = std::mem::take(&mut guard.unread_bytes);
        let output = String::from_utf8_lossy(&bytes).to_string();
        Ok(ProcessOutput {
            proc_id: proc_id.to_string(),
            command: guard.command.clone(),
            output,
            running: guard.running,
            exit_code: guard.exit_code,
        })
    }

    /// Writes `input` bytes to the PTY stdin of `proc_id`.
    pub fn write_input(&self, proc_id: &str, input: &str) -> Result<(), String> {
        let handle = self.get_handle(proc_id)?;
        let mut writer = handle
            .writer
            .lock()
            .map_err(|_| "Process writer lock poisoned".to_string())?;
        writer
            .write_all(input.as_bytes())
            .map_err(|e| format!("Failed to write to PTY '{proc_id}': {e}"))?;
        writer
            .flush()
            .map_err(|e| format!("Failed to flush PTY '{proc_id}': {e}"))?;
        Ok(())
    }

    /// Terminates a running background process. Returns `Ok(true)` if it was running when killed,
    /// or `Ok(false)` if it had already exited.
    pub fn stop_process(&self, proc_id: &str) -> Result<bool, String> {
        let handle = self.get_handle(proc_id)?;
        let was_running = handle
            .state
            .lock()
            .map_err(|_| "Process state lock poisoned".to_string())?
            .running;

        if !was_running {
            return Ok(false);
        }

        handle
            .killer
            .lock()
            .map_err(|_| "Process killer lock poisoned".to_string())?
            .kill()
            .map_err(|e| format!("Failed to kill PTY process '{proc_id}': {e}"))?;

        if let Ok(mut guard) = handle.state.lock() {
            guard.running = false;
        }
        Ok(true)
    }

    /// Lists all tracked background processes (both active and exited).
    pub fn list_processes(&self) -> Vec<BackgroundProcessInfo> {
        let entries: Vec<(String, Arc<BackgroundProcessHandle>)> = {
            let Ok(map) = self.processes.lock() else {
                return Vec::new();
            };
            map.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
        };

        let mut list = Vec::with_capacity(entries.len());
        for (proc_id, handle) in entries {
            if let Ok(guard) = handle.state.lock() {
                list.push(BackgroundProcessInfo {
                    proc_id,
                    command: guard.command.clone(),
                    cwd: guard.cwd.clone(),
                    started_at: guard.started_at,
                    running: guard.running,
                    exit_code: guard.exit_code,
                });
            }
        }
        list.sort_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| a.proc_id.cmp(&b.proc_id)));
        list
    }

    fn get_handle(&self, proc_id: &str) -> Result<Arc<BackgroundProcessHandle>, String> {
        let map = self
            .processes
            .lock()
            .map_err(|_| "PTY manager lock poisoned".to_string())?;
        map.get(proc_id)
            .cloned()
            .ok_or_else(|| format!("Unknown background process ID: '{proc_id}'"))
    }
}

/// Runs `command` via `/bin/zsh -lc` inside a real PTY in `cwd`, streaming live output chunks to
/// `on_chunk` as they arrive and returning `(exit_code, combined_output)`.
pub async fn run_pty_command<F>(
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    mut on_chunk: F,
) -> Result<(i32, String), String>
where
    F: FnMut(&str) + Send,
{
    if !cwd.exists() {
        return Err(format!("Working directory does not exist: {}", cwd.display()));
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: DEFAULT_PTY_ROWS,
            cols: DEFAULT_PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    let cmd_builder = build_zsh_command(command, cwd);
    let mut child = pair
        .slave
        .spawn_command(cmd_builder)
        .map_err(|e| format!("Failed to spawn PTY command: {e}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;
    let mut killer = child.clone_killer();
    let master_guard = pair.master;

    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if chunk_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if is_pty_eof(&e) => break,
                Err(_) => break,
            }
        }
    });

    let mut wait_handle = tokio::task::spawn_blocking(move || child.wait());
    let effective_timeout = if timeout_secs == 0 { 300 } else { timeout_secs };
    let deadline = tokio::time::sleep(Duration::from_secs(effective_timeout));
    tokio::pin!(deadline);

    let mut combined = String::new();
    let mut exit_code: Option<i32> = None;

    loop {
        if exit_code.is_some() {
            // Child already exited; drain any remaining buffered PTY output briefly.
            let drain_deadline = tokio::time::sleep(Duration::from_millis(200));
            tokio::pin!(drain_deadline);
            loop {
                tokio::select! {
                    maybe_bytes = chunk_rx.recv() => {
                        match maybe_bytes {
                            Some(bytes) => emit_chunk(&bytes, &mut combined, &mut on_chunk),
                            None => break,
                        }
                    }
                    () = &mut drain_deadline => break,
                }
            }
            break;
        }

        tokio::select! {
            maybe_bytes = chunk_rx.recv() => {
                match maybe_bytes {
                    Some(bytes) => emit_chunk(&bytes, &mut combined, &mut on_chunk),
                    None => {
                        // Reader hit EOF; wait for child exit status.
                        let status = wait_handle
                            .await
                            .map_err(|e| format!("PTY wait task panicked: {e}"))?
                            .map_err(|e| format!("PTY child wait failed: {e}"))?;
                        exit_code = Some(status.exit_code() as i32);
                        break;
                    }
                }
            }
            wait_res = &mut wait_handle => {
                let status = wait_res
                    .map_err(|e| format!("PTY wait task panicked: {e}"))?
                    .map_err(|e| format!("PTY child wait failed: {e}"))?;
                exit_code = Some(status.exit_code() as i32);
            }
            () = &mut deadline => {
                let _ = killer.kill();
                drop(master_guard);
                return Err(format!(
                    "PTY command timed out after {effective_timeout}s. Partial output:\n{combined}"
                ));
            }
        }
    }

    drop(master_guard);
    Ok((exit_code.unwrap_or(-1), combined))
}

fn build_zsh_command(command: &str, cwd: &Path) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("/bin/zsh");
    cmd.args(["-lc", command]);
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    cmd
}

fn emit_chunk<F: FnMut(&str)>(bytes: &[u8], combined: &mut String, on_chunk: &mut F) {
    let text = String::from_utf8_lossy(bytes);
    if !text.is_empty() {
        combined.push_str(&text);
        on_chunk(&text);
    }
}

fn is_pty_eof(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof)
        || err.raw_os_error() == Some(5) // EIO on macOS/Linux when PTY slave closes
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skip_if_pty_unavailable(err: &str) -> bool {
        let lower = err.to_ascii_lowercase();
        lower.contains("permissiondenied")
            || lower.contains("operation not permitted")
            || lower.contains("openpty")
            || lower.contains("failed to open pty")
    }

    #[tokio::test]
    async fn runs_short_command_via_pty_and_streams_output() {
        let cwd = std::env::temp_dir();
        let mut chunks = Vec::new();
        let result = run_pty_command("echo rursor_pty_ok", &cwd, 10, |chunk| {
            chunks.push(chunk.to_string());
        })
        .await;
        let (code, output) = match result {
            Ok(v) => v,
            Err(e) if skip_if_pty_unavailable(&e) => {
                eprintln!("skipping PTY test (restricted environment): {e}");
                return;
            }
            Err(e) => panic!("unexpected PTY error: {e}"),
        };

        assert_eq!(code, 0);
        assert!(output.contains("rursor_pty_ok"));
        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn returns_nonzero_exit_code_from_pty_command() {
        let cwd = std::env::temp_dir();
        let result = run_pty_command("echo failing_cmd && exit 7", &cwd, 10, |_| {}).await;
        let (code, output) = match result {
            Ok(v) => v,
            Err(e) if skip_if_pty_unavailable(&e) => {
                eprintln!("skipping PTY test (restricted environment): {e}");
                return;
            }
            Err(e) => panic!("unexpected PTY error: {e}"),
        };

        assert_eq!(code, 7);
        assert!(output.contains("failing_cmd"));
    }

    #[test]
    fn manages_background_process_start_read_write_and_stop() {
        let manager = PtyManager::new();
        let cwd: PathBuf = std::env::temp_dir();

        let info = match manager.start_background("echo bg_started && cat", &cwd) {
            Ok(v) => v,
            Err(e) if skip_if_pty_unavailable(&e) => {
                eprintln!("skipping background PTY test (restricted environment): {e}");
                return;
            }
            Err(e) => panic!("unexpected PTY error: {e}"),
        };
        assert!(info.running);
        assert_eq!(manager.list_processes().len(), 1);

        // Poll briefly until the initial echo output arrives in the PTY buffer
        let mut accumulated = String::new();
        for _ in 0..30 {
            let out = manager.read_output(&info.proc_id).unwrap();
            accumulated.push_str(&out.output);
            if accumulated.contains("bg_started") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            accumulated.contains("bg_started"),
            "Expected 'bg_started' in PTY output, got: {accumulated:?}"
        );

        manager.write_input(&info.proc_id, "pty_ping\n").unwrap();
        let mut echoed = String::new();
        for _ in 0..30 {
            let out = manager.read_output(&info.proc_id).unwrap();
            echoed.push_str(&out.output);
            if echoed.contains("pty_ping") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            echoed.contains("pty_ping"),
            "Expected 'pty_ping' echoed in PTY output, got: {echoed:?}"
        );

        let stopped = manager.stop_process(&info.proc_id).unwrap();
        assert!(stopped);

        let list = manager.list_processes();
        assert_eq!(list.len(), 1);
        assert!(!list[0].running);
    }
}
