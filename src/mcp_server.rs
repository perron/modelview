//! Stdio MCP server. Every tool is a thin wrapper over the CLI. Paths must
//! sit inside a folder given with `--allow` or the config file. The server
//! does not execute code from a model and does not write unless a tool is
//! given an output path that is also inside an allowed folder.

use std::fs;
use std::path::{Path, PathBuf};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::cli::render_png_bytes;
use crate::error::error_json;
use crate::info;
use crate::load::load_isolated;
use crate::scene::Scene;
use crate::MvError;

#[derive(Clone)]
pub struct ModelServer {
    allow: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PathArgs {
    /// Path to a model inside an allowed folder.
    path: String,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ValidateArgs {
    path: String,
    #[serde(default)]
    strict: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RenderArgs {
    path: String,
    #[serde(default)]
    view: Option<String>,
    #[serde(default)]
    yaw: Option<f32>,
    #[serde(default)]
    pitch: Option<f32>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    anim: Option<String>,
    #[serde(default)]
    time: Option<f32>,
    /// Write the PNG here as well. Must be inside an allowed folder.
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TurntableArgs {
    path: String,
    #[serde(default)]
    frames: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    anim: Option<String>,
    #[serde(default)]
    time: Option<f32>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindArgs {
    /// Directory to search. Omit to search every allowed folder.
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    recursive: Option<bool>,
}

#[tool_router]
impl ModelServer {
    pub fn new(allow: Vec<PathBuf>) -> Self {
        Self { allow }
    }

    #[tool(
        description = "Summarize a 3D model: counts, bounds, materials, textures, bones, and animation takes. Read-only."
    )]
    fn model_info(&self, Parameters(args): Parameters<PathArgs>) -> CallToolResult {
        self.with_scene(&args.path, |scene| {
            text_ok(info::info_json(
                scene,
                args.detail.as_deref().unwrap_or("all"),
            ))
        })
    }

    #[tool(
        description = "Report missing textures (with every path searched), degenerate geometry, unit and axis notes, and unsupported features. Read-only."
    )]
    fn model_validate(&self, Parameters(args): Parameters<ValidateArgs>) -> CallToolResult {
        self.with_scene(&args.path, |scene| {
            let mut value = info::validate_json(scene);
            if args.strict.unwrap_or(false) {
                if let Some(obj) = value.as_object_mut() {
                    let warnings = obj
                        .get("warning_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if warnings > 0 {
                        obj.insert("strict".into(), serde_json::json!(true));
                    }
                }
            }
            text_ok(value)
        })
    }

    #[tool(
        description = "Render a PNG of the model from a preset view or yaw/pitch in degrees. Returns the PNG. If output is set, also writes that path, which must be inside an allowed folder."
    )]
    fn model_render(&self, Parameters(args): Parameters<RenderArgs>) -> CallToolResult {
        self.with_scene(&args.path, |scene| {
            let view = args.view.as_deref().unwrap_or("three-quarter");
            let mode = args.mode.as_deref().unwrap_or("shaded");
            let width = args.width.unwrap_or(512).clamp(16, 2048);
            let height = args.height.unwrap_or(512).clamp(16, 2048);
            match render_png_bytes(
                scene,
                view,
                args.yaw,
                args.pitch,
                width,
                height,
                mode,
                args.anim.as_deref(),
                args.time,
            ) {
                Ok((png, mut meta)) => {
                    if let Some(output) = &args.output {
                        if let Err(err) = self.write_allowed(Path::new(output), &png) {
                            return tool_error(err);
                        }
                        if let Some(obj) = meta.as_object_mut() {
                            obj.insert("output".into(), serde_json::json!(output));
                        }
                    }
                    image_ok(meta, &png)
                }
                Err(err) => tool_error(err),
            }
        })
    }

    #[tool(
        description = "Render an orbit contact sheet as one PNG. Read-only unless output is set, in which case that path must be inside an allowed folder."
    )]
    fn model_turntable(&self, Parameters(args): Parameters<TurntableArgs>) -> CallToolResult {
        self.with_scene(&args.path, |scene| {
            let frames = args.frames.unwrap_or(8).clamp(1, 32);
            let size = args.size.unwrap_or(256).clamp(32, 1024);
            let mode = args.mode.as_deref().unwrap_or("shaded");
            let (anim, time) = match scene.resolve_anim(args.anim.as_deref(), args.time) {
                Ok(v) => v,
                Err(err) => return tool_error(err),
            };
            let mode_parsed = match crate::render::RenderMode::parse(mode) {
                Ok(m) => m,
                Err(err) => return tool_error(err),
            };
            let mut renderer = match crate::render::Renderer::new() {
                Ok(r) => r,
                Err(err) => return tool_error(err),
            };
            let columns = (frames as f32).sqrt().ceil() as u32;
            let rows = frames.div_ceil(columns);
            let mut sheet = image::RgbaImage::new(columns * size, rows * size);
            let mut filled = false;
            for i in 0..frames {
                let image = match renderer.render_rgba(&crate::render::FrameRequest {
                    scene,
                    width: size,
                    height: size,
                    yaw: i as f32 * (360.0 / frames as f32),
                    pitch: 20.0,
                    mode: mode_parsed,
                    anim,
                    time,
                    uv_set: 0,
                    bone: None,
                    show_skeleton: scene.bone_count() > 0,
                    show_grid: true,
                    distance_scale: 1.0,
                    target_offset: [0.0, 0.0, 0.0],
                }) {
                    Ok(image) => image,
                    Err(err) => return tool_error(err),
                };
                if !filled {
                    let bg = *image.get_pixel(0, 0);
                    for pixel in sheet.pixels_mut() {
                        *pixel = bg;
                    }
                    filled = true;
                }
                image::imageops::overlay(
                    &mut sheet,
                    &image,
                    ((i % columns) * size) as i64,
                    ((i / columns) * size) as i64,
                );
            }
            let png = match crate::render::encode_png(&sheet) {
                Ok(png) => png,
                Err(err) => return tool_error(err),
            };
            let mut meta = serde_json::json!({
                "schema": "modelview/turntable/v1",
                "ok": true,
                "frames": frames,
                "width": sheet.width(),
                "height": sheet.height(),
                "adapter": renderer.adapter_name(),
            });
            if let Some(output) = &args.output {
                if let Err(err) = self.write_allowed(Path::new(output), &png) {
                    return tool_error(err);
                }
                meta["output"] = serde_json::json!(output);
            }
            image_ok(meta, &png)
        })
    }

    #[tool(description = "List animation takes on a model. Read-only.")]
    fn model_list_animations(&self, Parameters(args): Parameters<PathArgs>) -> CallToolResult {
        self.with_scene(&args.path, |scene| text_ok(info::animations_json(scene)))
    }

    #[tool(
        description = "Find model files under an allowed folder. Returns a quick FBX header (version, ascii or binary) when it can be read without parsing the file. Read-only."
    )]
    fn model_find(&self, Parameters(args): Parameters<FindArgs>) -> CallToolResult {
        let recursive = args.recursive.unwrap_or(true);
        let roots = if let Some(dir) = &args.dir {
            match self.permit(Path::new(dir), true) {
                Ok(path) => vec![path],
                Err(err) => return tool_error(err),
            }
        } else if self.allow.is_empty() {
            return tool_error(MvError::permission("no folders are allowed; pass --allow"));
        } else {
            self.allow.clone()
        };
        let mut found = Vec::new();
        for root in roots {
            walk(
                &root,
                recursive,
                args.pattern.as_deref(),
                &self.allow,
                &mut found,
            );
            if found.len() >= 2000 {
                break;
            }
        }
        text_ok(serde_json::json!({
            "schema": "modelview/find/v1",
            "ok": true,
            "count": found.len(),
            "files": found,
        }))
    }

    fn with_scene(&self, path: &str, f: impl FnOnce(&Scene) -> CallToolResult) -> CallToolResult {
        let path = match self.permit(Path::new(path), true) {
            Ok(path) => path,
            Err(err) => return tool_error(err),
        };
        match load_isolated(&path, std::time::Duration::from_secs(30), 2048) {
            Ok(scene) => f(&scene),
            Err(err) => tool_error(err),
        }
    }

    fn permit(&self, path: &Path, must_exist: bool) -> Result<PathBuf, MvError> {
        if self.allow.is_empty() {
            return Err(MvError::permission(
                "no folders are allowed; start modelview mcp with --allow",
            ));
        }
        let full = if path.exists() {
            path.canonicalize().map_err(MvError::io)?
        } else if must_exist {
            return Err(MvError::file_not_found(path.display()));
        } else if let Some(parent) = path.parent() {
            let name = path
                .file_name()
                .ok_or_else(|| MvError::invalid("output path has no file name"))?;
            if !parent.exists() {
                return Err(MvError::file_not_found(parent.display()));
            }
            parent.canonicalize().map_err(MvError::io)?.join(name)
        } else {
            return Err(MvError::invalid("empty path"));
        };
        if self.allow.iter().any(|root| within(&full, root)) {
            Ok(full)
        } else {
            Err(MvError::permission(format!(
                "{} is outside the allowed folders",
                path.display()
            )))
        }
    }

    fn write_allowed(&self, path: &Path, bytes: &[u8]) -> Result<(), MvError> {
        let full = self.permit(path, false)?;
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(MvError::io)?;
        }
        fs::write(&full, bytes).map_err(MvError::io)?;
        Ok(())
    }
}

#[tool_handler(
    instructions = "Inspect 3D models (FBX, glTF/GLB, OBJ, STL, PLY). Read-only: paths must be inside the folders given to `modelview mcp --allow`. A tool writes a file only when you pass an output path, and that path must also be inside an allowed folder. The server does not run code from model files."
)]
impl ServerHandler for ModelServer {}

pub fn run(mut allow: Vec<PathBuf>, config: Option<PathBuf>) -> crate::Result<()> {
    if let Some(path) = config {
        let text = fs::read_to_string(&path).map_err(MvError::io)?;
        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|err| MvError::invalid(err.to_string()))?;
        if let Some(list) = parsed.get("allow").and_then(|v| v.as_array()) {
            for item in list {
                if let Some(dir) = item.as_str() {
                    allow.push(PathBuf::from(dir));
                }
            }
        }
    }
    if allow.is_empty() {
        return Err(MvError::invalid(
            "mcp requires at least one --allow directory or a config file with an allow list",
        ));
    }
    let allow = allow
        .into_iter()
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();
    let server = ModelServer::new(allow);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| MvError::io(err.to_string()))?;
    runtime.block_on(async {
        let service = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|err| MvError::io(format!("mcp startup failed: {err}")))?;
        service
            .waiting()
            .await
            .map_err(|err| MvError::io(format!("mcp server stopped: {err}")))?;
        Ok(())
    })
}

fn text_ok(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    )])
}

fn image_ok(value: serde_json::Value, png: &[u8]) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png);
    CallToolResult::success(vec![
        ContentBlock::text(text),
        ContentBlock::image(encoded, "image/png"),
    ])
}

fn tool_error(err: MvError) -> CallToolResult {
    let body = serde_json::to_string_pretty(&error_json(&err)).unwrap_or_else(|_| err.to_string());
    CallToolResult::error(vec![ContentBlock::text(body)])
}

fn within(path: &Path, root: &Path) -> bool {
    let path = components(path);
    let root = components(root);
    path.len() >= root.len() && path[..root.len()] == root[..]
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

fn walk(
    dir: &Path,
    recursive: bool,
    pattern: Option<&str>,
    allow: &[PathBuf],
    out: &mut Vec<serde_json::Value>,
) {
    if out.len() >= 2000 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 2000 {
            return;
        }
        let path = entry.path();
        let Ok(canon) = path.canonicalize() else {
            continue;
        };
        if !allow.iter().any(|root| within(&canon, root)) {
            continue;
        }
        if path.is_dir() {
            if recursive {
                walk(&canon, true, pattern, allow, out);
            }
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(pattern) = pattern {
            if !name
                .to_ascii_lowercase()
                .contains(&pattern.to_ascii_lowercase())
            {
                continue;
            }
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "fbx" | "gltf" | "glb" | "obj" | "stl" | "ply") {
            continue;
        }
        let mut item = serde_json::json!({
            "path": canon.display().to_string(),
            "name": name,
            "bytes": entry.metadata().map(|m| m.len()).unwrap_or(0),
            "format": ext,
        });
        if ext == "fbx" {
            if let Some((ascii, version)) = fbx_header(&canon) {
                item["fbx_ascii"] = serde_json::json!(ascii);
                item["fbx_version"] = serde_json::json!(version);
            }
        }
        out.push(item);
    }
}

fn fbx_header(path: &Path) -> Option<(bool, u32)> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = [0u8; 256];
    use std::io::Read;
    let n = file.read(&mut buf).ok()?;
    let buf = &buf[..n];
    if buf.starts_with(b"Kaydara FBX Binary") && buf.len() >= 27 {
        let version = u32::from_le_bytes(buf[23..27].try_into().ok()?);
        return Some((false, version));
    }
    let text = String::from_utf8_lossy(buf);
    let marker = "FBXVersion:";
    let rest = text.split(marker).nth(1)?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let version = digits.parse().ok()?;
    Some((true, version))
}
