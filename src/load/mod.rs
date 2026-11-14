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
static ACTIVE_TEMP: Mutex<Option<PathBuf>> = Mutex::new(None);

fn stop_requested() -> bool {
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
    // Desktop/Documents/Downloads file. Link or clone it here so the prompt
    // names this app and we do not write a second full copy. Other platforms
    // hand the worker the original path.
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

/// Link the model into the temp directory. A hard link (or an APFS clone on
/// macOS) avoids writing the file out a second time. A full copy is the fallback.
fn stage_model(original: &Path, dest: &Path) -> std::io::Result<()> {
    if fs::hard_link(original, dest).is_ok() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        if clone_file(original, dest).is_ok() {
            return Ok(());
        }
    }
    fs::copy(original, dest).map(|_| ())
}

#[cfg(target_os = "macos")]
fn clone_file(original: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let src = std::ffi::CString::new(original.as_os_str().as_bytes())
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    let dst = std::ffi::CString::new(dest.as_os_str().as_bytes())
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    let rc = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
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

fn install_stop_handler() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_stop as *const () as libc::sighandler_t;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        });
    }
}

#[cfg(unix)]
extern "C" fn handle_stop(_: libc::c_int) {
    STOP_LOAD.store(true, Ordering::Relaxed);
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
        let _ = fs::copy(&from, &to);
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

pub fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

pub fn default_memory_mb() -> u64 {
    DEFAULT_MEMORY_MB
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.fbx"), b"leftover").unwrap();
        let mine = std::env::temp_dir().join(format!("modelview-{}-live", std::process::id()));
        let _ = fs::remove_dir_all(&mine);
        fs::create_dir_all(&mine).unwrap();
        sweep_stale_temp_dirs();
        assert!(!dir.exists(), "dead owner's temp dir should be gone");
        assert!(mine.exists(), "this process's temp dir should stay");
        let _ = fs::remove_dir_all(&mine);
    }

    #[test]
    fn stage_model_links_instead_of_copying_bytes() {
        let dir = std::env::temp_dir().join(format!("mv-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("model.fbx");
        fs::write(&src, b"not-a-real-fbx").unwrap();
        let dest = dir.join("linked.fbx");
        stage_model(&src, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"not-a-real-fbx");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&src).unwrap().ino(),
                fs::metadata(&dest).unwrap().ino(),
                "stage_model should hard-link on the same volume"
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
