use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_modelview"))
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin())
        .args(args)
        .env(
            "MODELVIEW_SOFTWARE",
            std::env::var("MODELVIEW_SOFTWARE").unwrap_or_else(|_| "1".into()),
        )
        .output()
        .expect("spawn modelview");
    (
        out.status.code().unwrap_or(101),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn json_stdout(args: &[&str]) -> (i32, serde_json::Value) {
    let (code, stdout, stderr) = run(args);
    let value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!("stdout was not JSON ({err})\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });
    (code, value)
}

#[test]
fn info_validate_and_errors_are_versioned_json() {
    let (code, info) = json_stdout(&["info", "testdata/generated/cube.obj", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(info["schema"], "modelview/info/v1");
    assert_eq!(info["ok"], true);
    assert_eq!(info["counts"]["triangles"], 12);
    assert_eq!(info["textures"][0]["status"], "ok");

    let (code, ascii) = json_stdout(&[
        "info",
        "testdata/ufbx/blender_279_default_6100_ascii.fbx",
        "--json",
    ]);
    assert_eq!(code, 0);
    assert_eq!(ascii["fbx"]["version"], 6100);
    assert_eq!(ascii["fbx"]["ascii"], true);
    assert!(ascii["counts"]["triangles"].as_u64().unwrap() > 0);

    let (code, old) = json_stdout(&[
        "info",
        "testdata/ufbx/blender_279_sausage_6100_ascii.fbx",
        "--json",
    ]);
    assert_eq!(code, 0);
    assert_eq!(old["fbx"]["version"], 6100);
    assert!(old["counts"]["bones"].as_u64().unwrap() >= 3);
    assert!(old["counts"]["animations"].as_u64().unwrap() >= 2);

    let (code, bin_fbx) = json_stdout(&[
        "info",
        "testdata/ufbx/blender_279_sausage_7400_binary.fbx",
        "--json",
    ]);
    assert_eq!(code, 0);
    assert_eq!(bin_fbx["fbx"]["ascii"], false);
    assert!(bin_fbx["counts"]["skins"].as_u64().unwrap() >= 1);

    let (code, v77) = json_stdout(&[
        "info",
        "testdata/ufbx/synthetic_direct_by_polygon_7700_ascii.fbx",
        "--json",
    ]);
    assert_eq!(code, 0);
    assert_eq!(v77["fbx"]["version"], 7700);

    let (code, glb) = json_stdout(&[
        "info",
        "testdata/khronos/BoxTextured/BoxTextured.glb",
        "--json",
    ]);
    assert_eq!(code, 0);
    assert!(glb["textures"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["embedded"] == true && t["status"] == "ok"));

    let (code, embedded) = json_stdout(&[
        "info",
        "testdata/ufbx/blender_293_embedded_textures_7400_binary.fbx",
        "--json",
    ]);
    assert_eq!(code, 0);
    assert!(embedded["textures"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["embedded"] == true && t["status"] == "ok" && t["width"].as_u64().unwrap() > 0));

    let (code, missing) =
        json_stdout(&["validate", "testdata/generated/missing_tex.obj", "--json"]);
    assert_eq!(code, 0);
    let codes: Vec<&str> = missing["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"TEXTURE_MISSING"));
    assert!(codes.contains(&"TEXTURE_ABSOLUTE_PATH"));
    assert!(missing["problems"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["code"] == "TEXTURE_MISSING" && p["searched"].as_array().unwrap().len() > 0));

    let (code, bad) = json_stdout(&["info", "testdata/generated/corrupt.fbx", "--json"]);
    assert_eq!(code, 2);
    assert_eq!(bad["code"], "PARSE_FAILED");

    let (code, gone) = json_stdout(&["info", "testdata/no-such-file.fbx", "--json"]);
    assert_eq!(code, 3);
    assert_eq!(gone["code"], "FILE_NOT_FOUND");
}

#[test]
fn loader_timeout_does_not_hang_the_parent() {
    let mut child = Command::new(bin())
        .args([
            "info",
            "testdata/generated/cube.obj",
            "--json",
            "--timeout",
            "1",
        ])
        .env("MODELVIEW_WORKER_SLEEP_MS", "8000")
        .env("MODELVIEW_SOFTWARE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    let status = child.wait().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "parent waited {:?}",
        start.elapsed()
    );
    assert_eq!(status.code(), Some(3));
}

#[test]
fn render_matches_lavapipe_goldens_and_export_reloads() {
    let dir = std::env::temp_dir().join(format!("modelview-render-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let cases = [
        (
            "cube_three_quarter.png",
            "testdata/generated/cube.obj",
            "three-quarter",
            "shaded",
        ),
        (
            "box_front.png",
            "testdata/khronos/BoxTextured/BoxTextured.glb",
            "front",
            "shaded",
        ),
        (
            "uv_front.png",
            "testdata/generated/cube.obj",
            "front",
            "uv-checker",
        ),
    ];
    for (name, file, view, mode) in cases {
        let out = dir.join(name);
        let out_s = out.to_str().unwrap().to_string();
        let (code, report) = json_stdout(&[
            "render", file, "--view", view, "--size", "128x128", "--mode", mode, "-o", &out_s,
            "--json",
        ]);
        assert_eq!(code, 0, "{name} {report}");
        assert_eq!(report["schema"], "modelview/render/v1");
        assert!(report["adapter"].as_str().unwrap().len() > 0, "{report}");
        let image = image::open(&out).unwrap().to_rgba8();
        assert_eq!((image.width(), image.height()), (128, 128));
        assert!(color_spread(&image) > 8.0, "{name} looks blank");
        compare_golden(name, &image);
    }

    let glb = dir.join("sausage.glb");
    let (code, report) = {
        let args = [
            "export",
            "glb",
            "testdata/ufbx/blender_279_sausage_6100_ascii.fbx",
            "-o",
            glb.to_str().unwrap(),
            "--json",
        ];
        json_stdout(&args)
    };
    assert_eq!(code, 0, "{report}");
    let reloaded = modelview::load_in_process(&glb).expect("exported glb should load");
    assert!(reloaded.bone_count() >= 3);
    assert!(reloaded.animations.len() >= 2);
    assert!(reloaded.skins.len() >= 1);
    if let Some(validator) = gltf_validator() {
        let status = Command::new(validator)
            .arg(&glb)
            .status()
            .expect("gltf_validator");
        assert!(status.success(), "glTF-Validator rejected the export");
    } else if std::env::var("MODELVIEW_REQUIRE_VALIDATOR").ok().as_deref() == Some("1") {
        panic!("gltf_validator was not on PATH; Linux CI must install the Khronos binary");
    }
}

fn gltf_validator() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GLTF_VALIDATOR") {
        return Some(PathBuf::from(path));
    }
    let output = Command::new("gltf_validator").arg("-h").output().ok()?;
    if output.status.success() || output.status.code() == Some(1) {
        Some(PathBuf::from("gltf_validator"))
    } else {
        None
    }
}

fn compare_golden(name: &str, actual: &image::RgbaImage) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/golden")
        .join(name);
    if std::env::var_os("MODELVIEW_UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        actual.save(&path).unwrap();
        return;
    }
    let expected = image::open(&path)
        .unwrap_or_else(|err| {
            panic!("missing golden {name} ({err}); regenerate with MODELVIEW_UPDATE_GOLDEN=1")
        })
        .to_rgba8();
    let err = downsample_mae(&expected, actual, 16);
    let software = std::env::var("MODELVIEW_SOFTWARE").ok().as_deref() == Some("1");
    let limit = if software { 3.0 } else { 28.0 };
    assert!(
        err < limit,
        "{name} downsample MAE {err:.2} exceeds {limit} (software={software})"
    );
}

fn color_spread(image: &image::RgbaImage) -> f32 {
    let mut min = [255u8; 3];
    let mut max = [0u8; 3];
    for p in image.pixels() {
        for c in 0..3 {
            min[c] = min[c].min(p[c]);
            max[c] = max[c].max(p[c]);
        }
    }
    (0..3).map(|c| (max[c] - min[c]) as f32).sum()
}

fn downsample_mae(a: &image::RgbaImage, b: &image::RgbaImage, cells: u32) -> f32 {
    assert_eq!(a.dimensions(), b.dimensions());
    let (w, h) = a.dimensions();
    let mut acc = 0.0f32;
    let mut n = 0.0f32;
    for cy in 0..cells {
        for cx in 0..cells {
            let x0 = cx * w / cells;
            let x1 = (cx + 1) * w / cells;
            let y0 = cy * h / cells;
            let y1 = (cy + 1) * h / cells;
            let (mut sa, mut sb) = ([0.0; 3], [0.0; 3]);
            let mut count = 0.0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let pa = a.get_pixel(x, y);
                    let pb = b.get_pixel(x, y);
                    for c in 0..3 {
                        sa[c] += pa[c] as f32;
                        sb[c] += pb[c] as f32;
                    }
                    count += 1.0;
                }
            }
            for c in 0..3 {
                acc += ((sa[c] - sb[c]) / count).abs();
            }
            n += 3.0;
        }
    }
    acc / n
}

#[test]
fn mcp_lists_tools_and_refuses_paths_outside_allow() {
    let mut child = Command::new(bin())
        .args(["mcp", "--allow", "testdata/generated"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });
    let send = |stdin: &mut std::process::ChildStdin, value: serde_json::Value| {
        writeln!(stdin, "{value}").unwrap();
    };
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "modelview-test", "version": "0.1.0"}
            }
        }),
    );
    let init = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("initialize response");
    let init: serde_json::Value = serde_json::from_str(&init).unwrap();
    assert!(init.get("result").is_some(), "{init}");
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let tools = rx.recv_timeout(Duration::from_secs(10)).expect("tools");
    let tools: serde_json::Value = serde_json::from_str(&tools).unwrap();
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for required in [
        "model_info",
        "model_validate",
        "model_render",
        "model_turntable",
        "model_list_animations",
        "model_find",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "missing {required} in {names:?}"
        );
    }
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "model_info",
                "arguments": {"path": "testdata/ufbx/blender_279_default_6100_ascii.fbx"}
            }
        }),
    );
    let denied = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("denied call");
    assert!(denied.contains("PERMISSION_DENIED"), "{denied}");
    drop(stdin);
    let _ = child.wait_timeout_like();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn load_does_not_change_source_ctime_mtime_or_nlink() {
    let dir = std::env::temp_dir().join(format!("mv-stamp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("cube.obj");
    fs::copy("testdata/generated/cube.obj", &src).unwrap();
    // Copying can itself be followed by a timestamp settle. Capture after the copy.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        assert!(
            modelview::load::set_source_xattr(&src, "user.modelview.test", b"keep"),
            "could not attach a test xattr"
        );
    }
    let before = modelview::load::source_file_stamp(&src);
    let xattrs = modelview::load::source_xattrs(&src);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        assert!(
            xattrs
                .iter()
                .any(|(name, value)| name == "user.modelview.test" && value == b"keep"),
            "xattr reader missed the test attribute: {xattrs:?}"
        );
    }
    let (code, _) = json_stdout(&["info", src.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0);
    let after = modelview::load::source_file_stamp(&src);
    assert_eq!(
        before, after,
        "load changed source ctime/mtime/nlink: before {before:?} after {after:?}"
    );
    assert_eq!(
        modelview::load::source_xattrs(&src),
        xattrs,
        "load changed source extended attributes"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn chunked_vertex_buffers_match_a_single_buffer() {
    let scene = modelview::load_in_process(std::path::Path::new("testdata/generated/cube.obj"))
        .expect("cube");
    let mut renderer = modelview::render::Renderer::new().expect("renderer");
    let frame = modelview::render::FrameRequest {
        scene: &scene,
        width: 64,
        height: 64,
        yaw: 35.0,
        pitch: 25.0,
        mode: modelview::render::RenderMode::Shaded,
        anim: None,
        time: 0.0,
        uv_set: 0,
        bone: None,
        show_skeleton: false,
        show_grid: true,
        show_surface: true,
        distance_scale: 1.0,
        target_offset: [0.0, 0.0, 0.0],
    };
    let whole = renderer.render_rgba(&frame).expect("full buffer");
    // Three triangles of the 48-byte vertex per chunk, so the cube is split.
    renderer.set_vertex_budget(48 * 9);
    let chunked = renderer.render_rgba(&frame).expect("chunked buffers");
    assert_eq!(whole.dimensions(), chunked.dimensions());
    let mut different = 0u32;
    for (a, b) in whole.pixels().zip(chunked.pixels()) {
        if a != b {
            different += 1;
        }
    }
    assert_eq!(different, 0, "chunked drawing changed {different} pixels");
}

#[test]
fn a_blank_render_is_render_empty() {
    let scene = modelview::Scene::new("obj");
    let mut renderer = modelview::render::Renderer::new().expect("renderer");
    let image = renderer
        .render_rgba(&modelview::render::FrameRequest {
            scene: &scene,
            width: 64,
            height: 64,
            yaw: 0.0,
            pitch: 0.0,
            mode: modelview::render::RenderMode::Shaded,
            anim: None,
            time: 0.0,
            uv_set: 0,
            bone: None,
            show_skeleton: false,
            show_grid: false,
            show_surface: true,
            distance_scale: 1.0,
            target_offset: [0.0, 0.0, 0.0],
        })
        .expect("render");
    assert!(
        modelview::render::image_is_empty(&image),
        "a frame with nothing to draw must be a solid colour"
    );
    let err = modelview::MvError::new(
        "RENDER_EMPTY",
        "render produced a blank image. The mesh may be off camera or zero size",
    );
    assert_eq!(err.code, "RENDER_EMPTY");
    assert_eq!(modelview::error::exit_code(&err), 2);
}

#[cfg(unix)]
#[test]
fn sigterm_during_load_removes_the_temp_dir() {
    let mut child = Command::new(bin())
        .args(["info", "testdata/generated/cube.obj", "--json"])
        .env("MODELVIEW_STAGE_SLEEP_MS", "4000")
        .env("MODELVIEW_SOFTWARE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(300));
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let status = child.wait().unwrap();
    assert_ne!(
        status.code(),
        Some(0),
        "SIGTERM should not look like success"
    );
    let prefix = format!("modelview-{pid}-");
    let left = std::env::temp_dir()
        .read_dir()
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix));
    assert!(!left, "temp dir survived SIGTERM");
}

#[test]
fn load_worker_exits_when_the_parent_pipe_closes() {
    let out = std::env::temp_dir().join(format!("modelview-pipe-{}.bin", std::process::id()));
    let _ = fs::remove_file(&out);
    let mut child = Command::new(bin())
        .arg("--load-worker")
        .arg("testdata/generated/cube.obj")
        .arg(&out)
        .arg("256")
        .env("MODELVIEW_WORKER_SLEEP_MS", "8000")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");
    // Closing the write end is what happens when the app quits.
    drop(child.stdin.take());
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            panic!("worker kept running after the parent pipe closed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "worker should die when the parent disappears, not after the sleep"
    );
    let _ = fs::remove_file(&out);
}

trait WaitTimeoutLike {
    fn wait_timeout_like(&mut self);
}
impl WaitTimeoutLike for std::process::Child {
    fn wait_timeout_like(&mut self) {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            if self.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
