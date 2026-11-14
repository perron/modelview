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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::{MvError, Result};
use crate::scene::Scene;
use crate::validate;

pub use self::obj::parse_obj;

const DEFAULT_MEMORY_MB: u64 = 2048;

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
    Ok(scene)
}

/// Load by spawning this binary as `--load-worker`. A crash or a timeout
/// becomes [`MvError`] and does not kill the caller.
pub fn load_isolated(path: &Path, timeout: Duration, memory_mb: u64) -> Result<Scene> {
    if !path.is_file() {
        return Err(MvError::file_not_found(path.display()));
    }
    // Read the bytes in this process. On macOS a Desktop/Documents/Downloads
    // file raises the privacy prompt for whoever calls open(); doing it here
    // attributes the prompt to the app. The worker only sees a temp copy.
    let file_bytes = fs::read(path).map_err(MvError::io)?;
    let exe = std::env::current_exe().map_err(MvError::io)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let dir = std::env::temp_dir().join(format!("modelview-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&dir).map_err(MvError::io)?;
    let src = dir.join(format!("model.{ext}"));
    let out = dir.join("scene.bin");
    fs::write(&src, &file_bytes).map_err(MvError::io)?;
    if ext.eq_ignore_ascii_case("obj") {
        copy_obj_sidecars(path, &src);
    }
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
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
    };
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
        if scene.textures[index].embedded && scene.textures[index].status == "ok" {
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
}
