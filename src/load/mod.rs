mod fbx;
mod gltf;
mod obj;
mod ply;
mod stl;
pub mod textures;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::{MvError, Result};
use crate::scene::Scene;
use crate::validate;

pub use self::obj::parse_obj;

const DEFAULT_MEMORY_MB: u64 = 2048;

static STOP_LOAD: AtomicBool = AtomicBool::new(false);
static STOP_NOTIFIED: AtomicBool = AtomicBool::new(false);
static ACTIVE_TEMP: Mutex<Option<PathBuf>> = Mutex::new(None);

/// A stop signal (SIGTERM, SIGINT, or the Windows console equivalents) has
/// arrived. The window checks this and closes; a load still in progress exits.
pub fn stop_requested() -> bool {
    STOP_LOAD.load(Ordering::Relaxed)
}

/// Remove the temp directory the in-flight load created. Called when the
/// window exits, including while a Desktop read is still blocked.
pub fn cleanup_active_temp() {
    let Some(dir) = ACTIVE_TEMP.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    let _ = fs::remove_dir_all(dir);
}

fn remember_temp(dir: &Path) {
    if let Ok(mut guard) = ACTIVE_TEMP.lock() {
        *guard = Some(dir.to_path_buf());
    }
}

fn forget_temp(dir: &Path) {
    if let Ok(mut guard) = ACTIVE_TEMP.lock() {
        if guard.as_ref().is_some_and(|p| p == dir) {
            *guard = None;
        }
    }
}

/// Load in this process. The CLI and MCP server do not call this directly;
/// they spawn a child so a bad file cannot take the parent down. Tests call
/// it so a panic here fails the test instead of being swallowed.
pub fn load_in_process(path: &Path) -> Result<Scene> {
    if !path.is_file() {
        return Err(MvError::file_not_found(path.display()));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut scene = match ext.as_str() {
        "fbx" => fbx::load(path)?,
        "gltf" | "glb" => gltf::load(path)?,
        "obj" => obj::load(path)?,
        "stl" => stl::load(path)?,
        "ply" => ply::load(path)?,
        other => {
            return Err(MvError::parse(format!(
                "unsupported file type '.{other}' (expected fbx, gltf, glb, obj, stl, ply)"
            )));
        }
    };
    let meta = fs::metadata(path).map_err(MvError::io)?;
    scene.source.path = path.display().to_string();
    scene.source.file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    scene.source.size_bytes = meta.len();
    scene.source.sha256 = sha256_file(path)?;
    validate::add_geometry_problems(&mut scene);
    scene.recompute_bounds();
    validate::flag_unusual_scale(&mut scene);
    Ok(scene)
}

/// Load by spawning this binary as `--load-worker`. A crash or a timeout
/// becomes [`MvError`] and does not kill the caller.
pub fn load_isolated(path: &Path, timeout: Duration, memory_mb: u64) -> Result<Scene> {
    if !path.is_file() {
        return Err(MvError::file_not_found(path.display()));
    }
    // A killed parent cannot run its Drop guard. Sweep leftovers whose owning
    // pid is dead, even when the directory is only a few seconds old.
    install_stop_handler();
    sweep_stale_temp_dirs();
    let exe = std::env::current_exe().map_err(MvError::io)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let dir = std::env::temp_dir().join(format!("modelview-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|err| file_error(err, &dir))?;
    remember_temp(&dir);
    let _session = TempSession(dir.clone());
    pause_for_stage_test(&dir);
    let out = dir.join("scene.bin");
    // macOS privacy prompts are attributed to the process that opens a
    // Desktop/Documents/Downloads file, so this process opens it read-only
    // and writes a new temp file. clonefile and hard links are not used:
    // the first APFS clone updates the source ctime, and a hard link does
    // too. Other platforms hand the worker the original path and only read it.
    let src = if cfg!(target_os = "macos") {
        let src = dir.join(format!("model.{ext}"));
        if let Err(err) = stage_model(path, &src) {
            if stop_requested() {
                stop_and_exit(&dir);
            }
            return Err(file_error(err, path));
        }
        if ext.eq_ignore_ascii_case("obj") {
            copy_obj_sidecars(path, &src);
        }
        src
    } else {
        path.to_path_buf()
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--load-worker")
        .arg(&src)
        .arg(&out)
        .arg(memory_mb.max(64).to_string())
        // The write end of this pipe stays open until the child is dropped.
        // Process exit closes it too, and the worker treats EOF as "parent died".
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(MvError::io(err));
        }
    };
    let status = match wait_child(&mut child, timeout) {
        Ok(status) => status,
        Err(err) => {
            if stop_requested() {
                stop_and_exit(&dir);
            }
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
    };
    if stop_requested() {
        stop_and_exit(&dir);
    }
    // Dropping the child closes the pipe if the worker is somehow still alive.
    drop(child);
    let bytes = fs::read(&out).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    let mut scene = decode_worker_output(&bytes, status.success())?;
    scene.source.path = path.display().to_string();
    scene.source.file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if scene.source.format == "obj" {
        adopt_obj_materials(&mut scene, path);
    } else {
        refresh_external_textures(&mut scene, path);
    }
    Ok(scene)
}

/// Delete `modelview-<pid>-*` leftovers. A directory whose pid is no longer
/// alive is removed immediately. A live owner's directory waits 10 minutes,
/// and the listing's metadata is used so Windows does not have to open the
/// directory.
fn sweep_stale_temp_dirs() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(10 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let current = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("modelview-") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let dead_owner = temp_owner_pid(&name)
            .map(|pid| pid != current && !pid_alive(pid))
            .unwrap_or(false);
        let old = meta.modified().ok().map(|t| t < cutoff).unwrap_or(false);
        if !dead_owner && !old {
            continue;
        }
        if meta.is_dir() {
            let _ = fs::remove_dir_all(entry.path());
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `modelview-<pid>-<stamp>`. Names like `modelview-stale-...` have no pid.
fn temp_owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("modelview-")?;
    let pid = rest.split('-').next()?;
    let pid = pid.parse::<u32>().ok()?;
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
            fn GetExitCodeProcess(handle: *mut core::ffi::c_void, code: *mut u32) -> i32;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
            fn GetLastError() -> u32;
        }
        unsafe {
            // PROCESS_QUERY_LIMITED_INFORMATION. Access denied still means
            // some process owns the pid.
            let handle = OpenProcess(0x1000, 0, pid);
            if handle.is_null() {
                return GetLastError() == 5;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == 259
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

struct TempSession(PathBuf);

impl Drop for TempSession {
    fn drop(&mut self) {
        forget_temp(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn file_error(err: std::io::Error, path: &Path) -> MvError {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        MvError::permission(format!(
            "permission denied reading {}. If this file is in Desktop, Documents, or Downloads, allow modelview in the system's privacy settings",
            path.display()
        ))
    } else {
        MvError::io(format!("{}: {err}", path.display()))
    }
}

/// Copy `original` to `dest` by reading it and writing a new file.
///
/// `clonefile` on APFS updates the source inode's ctime the first time a
/// file is cloned, and a hard link updates ctime on every link. Neither is
/// used. `File::open` is read-only, and nothing here writes an xattr.
fn stage_model(original: &Path, dest: &Path) -> std::io::Result<()> {
    let bytes = read_only(original)?;
    fs::write(dest, &bytes)
}

fn read_only(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs::OpenOptions::new().read(true).write(false).open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn pause_for_stage_test(dir: &Path) {
    let Ok(text) = std::env::var("MODELVIEW_STAGE_SLEEP_MS") else {
        return;
    };
    let Ok(ms) = text.parse::<u64>() else {
        return;
    };
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        if stop_requested() {
            stop_and_exit(dir);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stop_and_exit(dir: &Path) -> ! {
    forget_temp(dir);
    let _ = fs::remove_dir_all(dir);
    std::process::exit(130);
}

/// Install the process-wide stop handler. Safe to call more than once.
/// The handler only stores a flag and wakes a waiter. It does not exit:
/// the window has to see the flag and close, and a load in progress exits
/// from ordinary code once it notices the flag.
pub fn install_stop_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(unix)]
        unsafe {
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) == 0 {
                WAKE_READ.store(fds[0], Ordering::Relaxed);
                WAKE_WRITE.store(fds[1], Ordering::Relaxed);
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_stop as *const () as libc::sighandler_t;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        }
        #[cfg(windows)]
        unsafe {
            let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
            WAKE_EVENT.store(event, Ordering::Relaxed);
            // A new process group starts with Ctrl+C ignored. Turn that off,
            // then handle Ctrl+C, Ctrl+Break, and console close ourselves.
            SetConsoleCtrlHandler(None, 0);
            SetConsoleCtrlHandler(Some(console_ctrl), 1);
        }
    });
}

/// Block until a stop signal has been delivered. The window waits on this
/// from a side thread and then asks the event loop to repaint, so an idle
/// window notices the signal immediately.
pub fn block_until_stop() {
    if stop_requested() {
        return;
    }
    #[cfg(unix)]
    {
        let fd = WAKE_READ.load(Ordering::Relaxed);
        if fd >= 0 {
            let mut buf = [0u8; 1];
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), 1) };
                if n > 0 || stop_requested() {
                    return;
                }
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return;
            }
        }
    }
    #[cfg(windows)]
    {
        let event = WAKE_EVENT.load(Ordering::Relaxed);
        if !event.is_null() {
            unsafe { WaitForSingleObject(event, u32::MAX) };
            return;
        }
    }
    while !stop_requested() {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
static WAKE_READ: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static WAKE_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn handle_stop(_: libc::c_int) {
    STOP_LOAD.store(true, Ordering::Relaxed);
    if STOP_NOTIFIED.swap(true, Ordering::Relaxed) {
        return;
    }
    let fd = WAKE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}

/// Ctrl+C is 0, Ctrl+Break is 1, console close is 2, logoff is 5, and
/// shutdown is 6. Those are the Windows equivalents of SIGINT and SIGTERM.
#[cfg(windows)]
fn console_event_quits(ctrl: u32) -> bool {
    matches!(ctrl, 0 | 1 | 2 | 5 | 6)
}

#[cfg(windows)]
static WAKE_EVENT: std::sync::atomic::AtomicPtr<core::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateEventW(
        security: *mut core::ffi::c_void,
        manual_reset: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut core::ffi::c_void;
    fn SetEvent(handle: *mut core::ffi::c_void) -> i32;
    fn WaitForSingleObject(handle: *mut core::ffi::c_void, millis: u32) -> u32;
    fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

#[cfg(windows)]
extern "system" fn console_ctrl(ctrl: u32) -> i32 {
    if !console_event_quits(ctrl) {
        return 0;
    }
    STOP_LOAD.store(true, Ordering::Relaxed);
    let event = WAKE_EVENT.load(Ordering::Relaxed);
    if !event.is_null() {
        unsafe { SetEvent(event) };
    }
    // Closing the console, logging off, or shutting down ends the process
    // when this handler returns. The temp directory has to be gone by then.
    if matches!(ctrl, 2 | 5 | 6) {
        cleanup_active_temp();
    }
    1
}

/// OBJ materials and textures are read in this process, next to the original
/// file. The worker's temp copy does not include the `.mtl` or image files.
fn adopt_obj_materials(scene: &mut Scene, original: &Path) {
    let Ok(local) = obj::load(original) else {
        return;
    };
    if scene.meshes.len() == local.meshes.len() {
        for (mesh, src) in scene.meshes.iter_mut().zip(local.meshes.iter()) {
            mesh.material = src.material;
        }
    }
    scene.materials = local.materials;
    scene.textures = local.textures;
    scene
        .problems
        .retain(|p| p.code != "TEXTURE_MISSING" && p.code != "TEXTURE_ABSOLUTE_PATH");
    for problem in local.problems {
        if matches!(
            problem.code.as_str(),
            "TEXTURE_MISSING" | "TEXTURE_ABSOLUTE_PATH" | "TEXTURE_DECODE_FAILED"
        ) {
            scene.add_problem(problem);
        }
    }
}

/// `mtllib` files sit next to the OBJ. Copy them beside the temp copy so the
/// worker can see material and texture names. The bytes are read here.
fn copy_obj_sidecars(original: &Path, temp_obj: &Path) {
    let Ok(text) = fs::read_to_string(original) else {
        return;
    };
    let Some(dir) = original.parent() else {
        return;
    };
    let Some(temp_dir) = temp_obj.parent() else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("mtllib")
            .or_else(|| line.strip_prefix("MTLLIB"))
        else {
            continue;
        };
        let name = rest.trim();
        if name.is_empty() {
            continue;
        }
        let from = dir.join(name);
        let to = temp_dir.join(name);
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // A plain read and write. `fs::copy` on macOS clones the file and
        // can change the source ctime.
        if let Ok(bytes) = read_only(&from) {
            let _ = fs::write(&to, bytes);
        }
    }
}

/// Textures were resolved against the worker's temp copy. Search again next
/// to the file the user actually opened, in this process.
fn refresh_external_textures(scene: &mut Scene, original: &Path) {
    use crate::scene::Problem;
    scene
        .problems
        .retain(|p| p.code != "TEXTURE_MISSING" && p.code != "TEXTURE_ABSOLUTE_PATH");
    let mut notes = Vec::new();
    for index in 0..scene.textures.len() {
        // Embedded images are not files on disk. A failed decode stays
        // TEXTURE_DECODE_FAILED and is not also reported missing.
        if scene.textures[index].embedded {
            continue;
        }
        let referenced = scene.textures[index].referenced_path.clone();
        if referenced.is_empty() {
            continue;
        }
        let found = textures::resolve(original, &referenced);
        scene.textures[index].searched = found.searched.clone();
        if found.absolute_reference {
            notes.push(Problem::warning(
                "TEXTURE_ABSOLUTE_PATH",
                textures::absolute_path_message(&referenced, &found),
            ));
        }
        if let Some(path) = found.resolved.clone() {
            match fs::read(&path)
                .ok()
                .and_then(|b| textures::decode_bytes(&b))
            {
                Some((w, h, rgba)) => {
                    let tex = &mut scene.textures[index];
                    tex.resolved_path = Some(path.display().to_string());
                    tex.status = "ok".into();
                    tex.width = w;
                    tex.height = h;
                    tex.rgba = rgba;
                    continue;
                }
                None => {
                    scene.textures[index].status = "decode_failed".into();
                    notes.push(Problem::warning(
                        "TEXTURE_DECODE_FAILED",
                        format!("could not decode {}", path.display()),
                    ));
                    continue;
                }
            }
        }
        scene.textures[index].status = "missing".into();
        scene.textures[index].resolved_path = None;
        let mut problem = Problem::warning(
            "TEXTURE_MISSING",
            format!("texture '{referenced}' was not found"),
        );
        problem.path = Some(referenced);
        problem.searched = found.searched;
        notes.push(problem);
    }
    for problem in notes {
        scene.add_problem(problem);
    }
}

pub fn worker_main(args: &[String]) -> i32 {
    // Exit as soon as the parent's pipe closes, including when the app quits
    // while this process is blocked inside a file read.
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match std::io::Read::read(&mut stdin, &mut buf) {
                Ok(0) | Err(_) => std::process::exit(3),
                Ok(_) => {}
            }
        }
    });
    let Some(input) = args.get(1) else {
        return 2;
    };
    let Some(output) = args.get(2) else {
        return 2;
    };
    let memory_mb = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MEMORY_MB);
    limit_address_space(memory_mb);
    // Test hook: CI sets this so a healthy file still trips the parent timeout.
    if let Ok(ms) = std::env::var("MODELVIEW_WORKER_SLEEP_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
    let path = PathBuf::from(input);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_in_process(&path)));
    let encoded = match result {
        Ok(Ok(scene)) => match bincode::serialize(&scene) {
            Ok(bytes) => {
                let mut buf = b"MVOK".to_vec();
                buf.extend(bytes);
                buf
            }
            Err(err) => error_bytes(&MvError::parse(format!("could not serialize scene: {err}"))),
        },
        Ok(Err(err)) => error_bytes(&err),
        Err(payload) => error_bytes(&MvError::parse(format!(
            "loader panicked: {}",
            panic_message(payload.as_ref())
        ))),
    };
    match fs::File::create(output).and_then(|mut f| f.write_all(&encoded)) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("modelview worker: {err}");
            2
        }
    }
}

fn error_bytes(err: &MvError) -> Vec<u8> {
    let mut buf = b"MVER".to_vec();
    buf.extend(serde_json::to_vec(err).unwrap_or_else(|_| b"{}".to_vec()));
    buf
}

fn decode_worker_output(bytes: &[u8], success: bool) -> Result<Scene> {
    if bytes.len() >= 4 && &bytes[..4] == b"MVOK" {
        return bincode::deserialize(&bytes[4..])
            .map_err(|err| MvError::parse(format!("could not read loader output: {err}")));
    }
    if bytes.len() >= 4 && &bytes[..4] == b"MVER" {
        if let Ok(err) = serde_json::from_slice::<MvError>(&bytes[4..]) {
            return Err(err);
        }
    }
    if !success {
        return Err(MvError::parse(
            "loader process crashed while reading the file",
        ));
    }
    Err(MvError::parse("loader produced no scene"))
}

/// Wait until `child` exits, or kill it and return [`MvError`] with code `TIMEOUT`.
pub fn wait_child(child: &mut Child, timeout: Duration) -> Result<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(MvError::io)? {
            return Ok(status);
        }
        if stop_requested() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MvError::new(
                "INTERRUPTED",
                "loader was stopped because the process was asked to quit",
            ));
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MvError::timeout(format!(
                "loader exceeded {:.1}s and was stopped",
                timeout.as_secs_f32()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn limit_address_space(memory_mb: u64) {
    #[cfg(unix)]
    unsafe {
        let bytes = memory_mb.saturating_mul(1024 * 1024);
        let limit = libc::rlimit {
            rlim_cur: bytes,
            rlim_max: bytes,
        };
        let _ = libc::setrlimit(libc::RLIMIT_AS, &limit);
    }
    #[cfg(not(unix))]
    {
        let _ = memory_mb;
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(MvError::io)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(MvError::io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

#[cfg(test)]
fn long_sleeper() -> Child {
    #[cfg(windows)]
    {
        Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    #[cfg(not(windows))]
    {
        Command::new("sleep").arg("30").spawn().unwrap()
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Extended attributes as `(name, value)`, sorted by name.
///
/// Empty on Windows and on filesystems that do not support xattrs. A load
/// must not add, remove, or rewrite these. On macOS, `com.apple.lastuseddate`
/// and `com.apple.provenance` are written by the system (Spotlight / provenance),
/// not by modelview; modelview only opens the file read-only.
pub fn source_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        return unix_xattrs(path);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn unix_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return Vec::new();
    };
    let size = unsafe { libc::listxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe {
        libc::listxattr(
            cpath.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    xattr_values(&cpath, &buf)
}

#[cfg(target_os = "macos")]
fn unix_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return Vec::new();
    };
    let size = unsafe { libc::listxattr(cpath.as_ptr(), std::ptr::null_mut(), 0, 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe {
        libc::listxattr(
            cpath.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            0,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    xattr_values(&cpath, &buf)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn xattr_values(cpath: &std::ffi::CStr, listed: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for name in listed.split(|b| *b == 0) {
        if name.is_empty() {
            continue;
        }
        let Ok(name_c) = std::ffi::CString::new(name) else {
            continue;
        };
        let Some(name_s) = name_c.to_str().ok().map(str::to_string) else {
            continue;
        };
        let value = read_xattr(cpath, &name_c);
        out.push((name_s, value));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(target_os = "linux")]
fn read_xattr(cpath: &std::ffi::CStr, name: &std::ffi::CStr) -> Vec<u8> {
    let size = unsafe { libc::getxattr(cpath.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe {
        libc::getxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n < 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    buf
}

#[cfg(target_os = "macos")]
fn read_xattr(cpath: &std::ffi::CStr, name: &std::ffi::CStr) -> Vec<u8> {
    let size =
        unsafe { libc::getxattr(cpath.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe {
        libc::getxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            0,
        )
    };
    if n < 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    buf
}

/// Attach one extended attribute. Returns false when the filesystem refuses.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn set_source_xattr(path: &Path, name: &str, value: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let Ok(cname) = std::ffi::CString::new(name) else {
        return false;
    };
    let rc = unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::setxattr(
                cpath.as_ptr(),
                cname.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
            )
        }
        #[cfg(target_os = "macos")]
        {
            libc::setxattr(
                cpath.as_ptr(),
                cname.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
                0,
            )
        }
    };
    rc == 0
}

/// `(ctime, mtime, nlink)` of `path`. On Windows, `ctime` is the creation time,
/// which is what `stat` reports there. A load must leave this triple unchanged.
pub fn source_file_stamp(path: &Path) -> (u128, u128, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path).unwrap();
        let ctime = meta.ctime() as u128 * 1_000_000_000 + meta.ctime_nsec() as u128;
        let mtime = meta.mtime() as u128 * 1_000_000_000 + meta.mtime_nsec() as u128;
        return (ctime, mtime, meta.nlink());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let meta = fs::metadata(path).unwrap();
        return (
            meta.creation_time() as u128,
            meta.last_write_time() as u128,
            windows_nlink(path),
        );
    }
    #[cfg(not(any(unix, windows)))]
    {
        let meta = fs::metadata(path).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        (mtime, mtime, 1)
    }
}

#[cfg(windows)]
fn windows_nlink(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut core::ffi::c_void,
            disposition: u32,
            flags: u32,
            template: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn GetFileInformationByHandle(
            handle: *mut core::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    }
    #[repr(C)]
    struct ByHandleFileInformation {
        _attributes: u32,
        _creation_low: u32,
        _creation_high: u32,
        _access_low: u32,
        _access_high: u32,
        _write_low: u32,
        _write_high: u32,
        _volume: u32,
        _size_high: u32,
        _size_low: u32,
        links: u32,
        _index_high: u32,
        _index_low: u32,
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // FILE_READ_ATTRIBUTES, share read/write/delete, OPEN_EXISTING.
        let handle = CreateFileW(
            wide.as_ptr(),
            0x80,
            0x7,
            std::ptr::null_mut(),
            3,
            0,
            std::ptr::null_mut(),
        );
        if handle as isize == -1 {
            return 0;
        }
        let mut info = std::mem::zeroed::<ByHandleFileInformation>();
        let ok = GetFileInformationByHandle(handle, &mut info);
        CloseHandle(handle);
        if ok == 0 {
            0
        } else {
            info.links as u64
        }
    }
}

pub fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

pub fn default_memory_mb() -> u64 {
    DEFAULT_MEMORY_MB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn console_close_and_ctrl_c_both_ask_the_window_to_quit() {
        for ctrl in [0, 1, 2, 5, 6] {
            assert!(console_event_quits(ctrl), "control event {ctrl}");
        }
        assert!(!console_event_quits(3));
    }

    #[test]
    fn corrupt_files_fail_without_panicking() {
        let dir = std::env::temp_dir().join(format!("modelview-fuzz-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let names = ["a.fbx", "b.glb", "c.obj", "d.stl", "e.ply", "f.gltf"];
        for (i, name) in names.iter().enumerate() {
            let path = dir.join(name);
            let bytes: Vec<u8> = (0..128).map(|b| ((b * 17 + i * 13) & 0xff) as u8).collect();
            fs::write(&path, bytes).unwrap();
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_in_process(&path)));
            assert!(result.is_ok(), "{name} panicked");
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/generated");
        for name in [
            "corrupt.fbx",
            "corrupt.glb",
            "corrupt.obj",
            "corrupt.stl",
            "corrupt.ply",
        ] {
            let path = corpus.join(name);
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_in_process(&path)));
            let loaded = result.expect("panic");
            assert!(
                loaded.is_err(),
                "{name} unexpectedly loaded: {:?}",
                loaded.ok().map(|s| s.source.format)
            );
            assert_eq!(loaded.unwrap_err().code, "PARSE_FAILED");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_cube_formats_load() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/generated");
        let obj = load_in_process(&root.join("cube.obj")).unwrap();
        assert_eq!(obj.triangle_count(), 12);
        assert_eq!(obj.textures.len(), 1);
        assert_eq!(obj.textures[0].status, "ok");
        let glb = load_in_process(&root.join("embedded_cube.glb")).unwrap();
        assert_eq!(glb.triangle_count(), 12);
        assert!(glb.textures.iter().any(|t| t.embedded && t.status == "ok"));
        let case = load_in_process(&root.join("case_tex.obj")).unwrap();
        assert_eq!(
            case.textures[0].status, "ok",
            "searched {:?}",
            case.textures[0].searched
        );
        let missing = load_in_process(&root.join("missing_tex.obj")).unwrap();
        assert!(missing.problems.iter().any(|p| p.code == "TEXTURE_MISSING"));
        assert!(missing
            .problems
            .iter()
            .any(|p| p.code == "TEXTURE_ABSOLUTE_PATH"));
        let deg = load_in_process(&root.join("degenerate.obj")).unwrap();
        assert!(deg.problems.iter().any(|p| p.code == "DEGENERATE_GEOMETRY"));
    }

    #[test]
    fn stale_temp_dirs_are_removed_and_fresh_ones_stay() {
        let root = std::env::temp_dir();
        let stale = root.join(format!("modelview-stale-{}", std::process::id()));
        let fresh = root.join(format!("modelview-fresh-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stale);
        let _ = fs::remove_dir_all(&fresh);
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::write(stale.join("model.fbx"), b"leftover").unwrap();
        // `File::open` on a directory is access-denied on Windows. `set_times`
        // opens it with FILE_WRITE_ATTRIBUTES and FILE_FLAG_BACKUP_SEMANTICS.
        let times =
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(3600));
        fs::set_times(&stale, times).unwrap();
        sweep_stale_temp_dirs();
        assert!(!stale.exists(), "stale temp dir should be gone");
        assert!(fresh.exists(), "a temp dir from this run should stay");
        let _ = fs::remove_dir_all(&fresh);
    }

    #[test]
    fn a_dead_owners_temp_dir_is_removed_immediately() {
        let mut child = super::long_sleeper();
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        assert!(!pid_alive(pid), "pid {pid} still looks alive");
        let dir = std::env::temp_dir().join(format!("modelview-{pid}-dead"));
        let _ = fs::remove_dir_all(&dir);
        // A parallel test may sweep this dead-pid directory between the
        // create and the write. Plant the marker again if that happens.
        let marker = dir.join("model.fbx");
        let mut planted = false;
        for _ in 0..8 {
            fs::create_dir_all(&dir).unwrap();
            if fs::write(&marker, b"leftover").is_ok() {
                planted = true;
                break;
            }
        }
        assert!(planted, "could not plant the dead owner's temp dir");
        let mine = std::env::temp_dir().join(format!("modelview-{}-live", std::process::id()));
        let _ = fs::remove_dir_all(&mine);
        fs::create_dir_all(&mine).unwrap();
        sweep_stale_temp_dirs();
        assert!(!dir.exists(), "dead owner's temp dir should be gone");
        assert!(mine.exists(), "this process's temp dir should stay");
        let _ = fs::remove_dir_all(&mine);
    }

    #[test]
    fn stage_model_leaves_source_ctime_mtime_and_nlink_alone() {
        let dir = std::env::temp_dir().join(format!("mv-stage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("model.fbx");
        fs::write(&src, b"not-a-real-fbx").unwrap();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(
                super::set_source_xattr(&src, "user.modelview.test", b"keep"),
                "could not attach a test xattr"
            );
        }
        let before = super::source_file_stamp(&src);
        let xattrs_before = super::source_xattrs(&src);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(
                xattrs_before
                    .iter()
                    .any(|(name, value)| name == "user.modelview.test" && value == b"keep"),
                "xattr reader missed the test attribute: {xattrs_before:?}"
            );
        }
        let dest = dir.join("staged.fbx");
        stage_model(&src, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"not-a-real-fbx");
        assert_eq!(
            super::source_file_stamp(&src),
            before,
            "staging changed the source file's metadata"
        );
        assert_eq!(
            super::source_xattrs(&src),
            xattrs_before,
            "staging changed the source file's xattrs"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(before.2, 1, "source gained a link");
            assert_ne!(
                fs::metadata(&src).unwrap().ino(),
                fs::metadata(&dest).unwrap().ino(),
                "staging must not hard-link the source"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_denied_names_the_file() {
        let err = file_error(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            Path::new("/Users/x/Desktop/Kobold.fbx"),
        );
        assert_eq!(err.code, "PERMISSION_DENIED");
        assert!(err.message.contains("Kobold.fbx"), "{}", err.message);
        assert!(err.message.contains("privacy"), "{}", err.message);
    }
}
