use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::error::{error_json, exit_code, MvError};
use crate::export_glb;
use crate::info;
use crate::load::load_isolated;
use crate::render::{self, FrameRequest, RenderMode, Renderer};
use crate::scene::Scene;
use crate::validate;
use crate::Result;

#[derive(Parser)]
#[command(
    name = "modelview",
    version,
    about = "View FBX, glTF, OBJ, STL, and PLY files. A path with no subcommand opens the window."
)]
struct Cli {
    /// Print versioned JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    /// Seconds before a loader that has not finished is stopped.
    #[arg(long, global = true, default_value_t = 30)]
    timeout: u64,
    /// Address-space cap for the loader process, in megabytes (Unix).
    #[arg(long, global = true, default_value_t = 2048)]
    memory_mb: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarize a model.
    Info {
        file: PathBuf,
        /// Comma-separated sections: nodes, meshes, materials, textures, anims, or all.
        #[arg(long, default_value = "all")]
        detail: String,
    },
    /// Report missing textures, degenerate triangles, units, and unsupported data.
    Validate {
        file: PathBuf,
        /// Exit 1 when the file loaded with warnings.
        #[arg(long)]
        strict: bool,
    },
    /// Render a PNG without opening a window.
    Render {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value = "three-quarter")]
        view: String,
        /// Degrees. Overrides the yaw of --view.
        #[arg(long)]
        yaw: Option<f32>,
        /// Degrees. Overrides the pitch of --view.
        #[arg(long)]
        pitch: Option<f32>,
        /// WIDTHxHEIGHT, for example 1024x1024.
        #[arg(long, default_value = "512x512")]
        size: String,
        #[arg(long, default_value = "shaded")]
        mode: String,
        #[arg(long)]
        anim: Option<String>,
        /// Seconds into the chosen take.
        #[arg(long)]
        time: Option<f32>,
        /// UV set index for uv-checker mode.
        #[arg(long, default_value_t = 0)]
        uv: usize,
        /// Bone name for the skin-weight heatmap.
        #[arg(long)]
        bone: Option<String>,
    },
    /// Render a contact sheet of orbit frames.
    Turntable {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value_t = 8)]
        frames: u32,
        #[arg(long, default_value = "256x256")]
        size: String,
        #[arg(long, default_value = "shaded")]
        mode: String,
        #[arg(long)]
        anim: Option<String>,
        #[arg(long)]
        time: Option<f32>,
    },
    /// Write a file. `export glb` is the only format in v0.1.
    Export {
        #[command(subcommand)]
        command: ExportCommand,
    },
    /// List animation takes.
    ListAnimations { file: PathBuf },
    /// Start a stdio MCP server. Tools can only read paths under --allow.
    Mcp {
        /// Directory the server is allowed to read, and to write only when a tool is given an output path inside it.
        #[arg(long = "allow")]
        allow: Vec<PathBuf>,
        /// JSON file `{ "allow": ["..."] }` merged with --allow.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ExportCommand {
    /// Write a binary glTF file.
    Glb {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Export only this take. Omit to export every take.
        #[arg(long)]
        anim: Option<String>,
    },
}

const SUBCOMMANDS: &[&str] = &[
    "info",
    "validate",
    "render",
    "turntable",
    "export",
    "list-animations",
    "mcp",
    "help",
];

pub fn should_launch_gui(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--timeout" | "--memory-mb" => i += 2,
            "-h" | "--help" | "-V" | "--version" | "help" => return false,
            other if other.starts_with('-') => return false,
            other if SUBCOMMANDS.contains(&other) => return false,
            _ => return true,
        }
    }
    true
}

pub fn run(args: &mut [String]) -> i32 {
    let mut full = vec!["modelview".to_string()];
    full.extend(args.iter().cloned());
    let cli = match Cli::try_parse_from(full) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() { 2 } else { 0 };
        }
    };
    let json = cli.json || !io::stdout().is_terminal();
    let timeout = Duration::from_secs(cli.timeout.max(1));
    match cli.command {
        Command::Info { file, detail } => load_and(json, &file, timeout, cli.memory_mb, |scene| {
            let value = info::info_json(scene, &detail);
            emit(json, &value, &info::human_info(scene));
            0
        }),
        Command::Validate { file, strict } => {
            load_and(json, &file, timeout, cli.memory_mb, |scene| {
                let value = info::validate_json(scene);
                emit(json, &value, &info::human_validate(scene));
                let (errors, warnings, _) = validate::counts(scene);
                if errors > 0 {
                    2
                } else if strict && warnings > 0 {
                    1
                } else {
                    0
                }
            })
        }
        Command::Render {
            file,
            output,
            view,
            yaw,
            pitch,
            size,
            mode,
            anim,
            time,
            uv,
            bone,
        } => load_and(
            json,
            &file,
            timeout,
            cli.memory_mb,
            |scene| match render_one(
                scene,
                &output,
                &view,
                yaw,
                pitch,
                &size,
                &mode,
                anim.as_deref(),
                time,
                uv,
                bone.as_deref(),
            ) {
                Ok(value) => {
                    emit(json, &value, &format!("wrote {}\n", output.display()));
                    0
                }
                Err(err) => fail(json, &err),
            },
        ),
        Command::Turntable {
            file,
            output,
            frames,
            size,
            mode,
            anim,
            time,
        } => load_and(
            json,
            &file,
            timeout,
            cli.memory_mb,
            |scene| match render_turntable(
                scene,
                &output,
                frames,
                &size,
                &mode,
                anim.as_deref(),
                time,
            ) {
                Ok(value) => {
                    emit(json, &value, &format!("wrote {}\n", output.display()));
                    0
                }
                Err(err) => fail(json, &err),
            },
        ),
        Command::Export {
            command: ExportCommand::Glb { file, output, anim },
        } => load_and(
            json,
            &file,
            timeout,
            cli.memory_mb,
            |scene| match export_glb::export_glb(scene, &output, anim.as_deref()) {
                Ok(stats) => {
                    let value = serde_json::json!({
                        "schema": "modelview/export/v1",
                        "ok": true,
                        "output": output.display().to_string(),
                        "bytes": stats.bytes,
                        "meshes": stats.meshes,
                        "animations": stats.animations,
                    });
                    emit(
                        json,
                        &value,
                        &format!("wrote {} ({} bytes)\n", output.display(), stats.bytes),
                    );
                    0
                }
                Err(err) => fail(json, &err),
            },
        ),
        Command::ListAnimations { file } => {
            load_and(json, &file, timeout, cli.memory_mb, |scene| {
                let value = info::animations_json(scene);
                let mut human = String::new();
                if scene.animations.is_empty() {
                    human.push_str("no animations\n");
                }
                for anim in &scene.animations {
                    human.push_str(&format!(
                        "{}\t{:.3}\t{:.1}\n",
                        anim.name, anim.duration, anim.fps
                    ));
                }
                emit(json, &value, &human);
                0
            })
        }
        Command::Mcp { allow, config } => match crate::mcp_server::run(allow, config) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("{err}");
                exit_code(&err)
            }
        },
    }
}

fn load_and(
    json: bool,
    file: &PathBuf,
    timeout: Duration,
    memory_mb: u64,
    then: impl FnOnce(&Scene) -> i32,
) -> i32 {
    match load_isolated(file, timeout, memory_mb) {
        Ok(scene) => then(&scene),
        Err(err) => fail(json, &err),
    }
}

fn fail(json: bool, err: &MvError) -> i32 {
    if json {
        let _ = writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string_pretty(&error_json(err)).unwrap_or_default()
        );
    } else {
        eprintln!("{err}");
    }
    exit_code(err)
}

fn emit(json: bool, value: &serde_json::Value, human: &str) {
    if json {
        let _ = writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    } else {
        let _ = write!(io::stdout(), "{human}");
    }
}

fn parse_size(text: &str) -> Result<(u32, u32)> {
    let mut parts = text.split(['x', 'X', ',']);
    let w: u32 = parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| MvError::invalid(format!("bad size '{text}'")))?;
    let h: u32 = parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(w);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err(MvError::invalid("size must be between 1 and 8192"));
    }
    Ok((w, h))
}

fn render_one(
    scene: &Scene,
    output: &PathBuf,
    view: &str,
    yaw: Option<f32>,
    pitch: Option<f32>,
    size: &str,
    mode: &str,
    anim: Option<&str>,
    time: Option<f32>,
    uv: usize,
    bone: Option<&str>,
) -> Result<serde_json::Value> {
    let (w, h) = parse_size(size)?;
    let (view_yaw, view_pitch) = render::view_angles(view)?;
    let mode = RenderMode::parse(mode)?;
    let (anim_index, time) = scene.resolve_anim(anim, time)?;
    let bone_node = resolve_bone(scene, bone, mode)?;
    let mut renderer = Renderer::new()?;
    let image = renderer.render_rgba(&FrameRequest {
        scene,
        width: w,
        height: h,
        yaw: yaw.unwrap_or(view_yaw),
        pitch: pitch.unwrap_or(view_pitch),
        mode,
        anim: anim_index,
        time,
        uv_set: uv,
        bone: bone_node,
        show_skeleton: scene.bone_count() > 0,
        show_grid: true,
        distance_scale: 1.0,
        target_offset: [0.0, 0.0, 0.0],
    })?;
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(MvError::io)?;
        }
    }
    image
        .save(output)
        .map_err(|err| MvError::render(err.to_string()))?;
    Ok(serde_json::json!({
        "schema": "modelview/render/v1",
        "ok": true,
        "output": output.display().to_string(),
        "width": w,
        "height": h,
        "view": view,
        "yaw": yaw.unwrap_or(view_yaw),
        "pitch": pitch.unwrap_or(view_pitch),
        "mode": mode.as_str(),
        "animation": anim,
        "time": time,
        "adapter": renderer.adapter_name(),
    }))
}

fn render_turntable(
    scene: &Scene,
    output: &PathBuf,
    frames: u32,
    size: &str,
    mode: &str,
    anim: Option<&str>,
    time: Option<f32>,
) -> Result<serde_json::Value> {
    let frames = frames.clamp(1, 64);
    let (w, h) = parse_size(size)?;
    let mode = RenderMode::parse(mode)?;
    let (anim_index, time) = scene.resolve_anim(anim, time)?;
    let columns = (frames as f32).sqrt().ceil() as u32;
    let rows = frames.div_ceil(columns);
    let mut sheet = image::RgbaImage::new(columns * w, rows * h);
    let mut renderer = Renderer::new()?;
    let mut filled = false;
    for i in 0..frames {
        let yaw = i as f32 * (360.0 / frames as f32);
        let image = renderer.render_rgba(&FrameRequest {
            scene,
            width: w,
            height: h,
            yaw,
            pitch: 20.0,
            mode,
            anim: anim_index,
            time,
            uv_set: 0,
            bone: None,
            show_skeleton: scene.bone_count() > 0,
            show_grid: true,
            distance_scale: 1.0,
            target_offset: [0.0, 0.0, 0.0],
        })?;
        if !filled {
            let bg = *image.get_pixel(0, 0);
            for pixel in sheet.pixels_mut() {
                *pixel = bg;
            }
            filled = true;
        }
        let col = i % columns;
        let row = i / columns;
        image::imageops::overlay(&mut sheet, &image, (col * w) as i64, (row * h) as i64);
    }
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(MvError::io)?;
        }
    }
    sheet
        .save(output)
        .map_err(|err| MvError::render(err.to_string()))?;
    Ok(serde_json::json!({
        "schema": "modelview/turntable/v1",
        "ok": true,
        "output": output.display().to_string(),
        "frames": frames,
        "columns": columns,
        "rows": rows,
        "frame_width": w,
        "frame_height": h,
        "width": sheet.width(),
        "height": sheet.height(),
        "mode": mode.as_str(),
        "animation": anim,
        "time": time,
        "adapter": renderer.adapter_name(),
    }))
}

fn resolve_bone(scene: &Scene, bone: Option<&str>, mode: RenderMode) -> Result<Option<usize>> {
    if mode != RenderMode::Weights {
        if let Some(name) = bone {
            return scene
                .bone_index(name)
                .map(Some)
                .ok_or_else(|| MvError::invalid(format!("bone '{name}' not found")));
        }
        return Ok(None);
    }
    if let Some(name) = bone {
        return scene
            .bone_index(name)
            .map(Some)
            .ok_or_else(|| MvError::invalid(format!("bone '{name}' not found")));
    }
    Ok(scene.nodes.iter().position(|n| n.is_bone))
}

pub fn render_png_bytes(
    scene: &Scene,
    view: &str,
    yaw: Option<f32>,
    pitch: Option<f32>,
    width: u32,
    height: u32,
    mode: &str,
    anim: Option<&str>,
    time: Option<f32>,
) -> Result<(Vec<u8>, serde_json::Value)> {
    let (view_yaw, view_pitch) = render::view_angles(view)?;
    let mode = RenderMode::parse(mode)?;
    let (anim_index, time) = scene.resolve_anim(anim, time)?;
    let mut renderer = Renderer::new()?;
    let image = renderer.render_rgba(&FrameRequest {
        scene,
        width,
        height,
        yaw: yaw.unwrap_or(view_yaw),
        pitch: pitch.unwrap_or(view_pitch),
        mode,
        anim: anim_index,
        time,
        uv_set: 0,
        bone: if mode == RenderMode::Weights {
            scene.nodes.iter().position(|n| n.is_bone)
        } else {
            None
        },
        show_skeleton: scene.bone_count() > 0,
        show_grid: true,
        distance_scale: 1.0,
        target_offset: [0.0, 0.0, 0.0],
    })?;
    let png = render::encode_png(&image)?;
    let meta = serde_json::json!({
        "schema": "modelview/render/v1",
        "ok": true,
        "width": width,
        "height": height,
        "view": view,
        "yaw": yaw.unwrap_or(view_yaw),
        "pitch": pitch.unwrap_or(view_pitch),
        "mode": mode.as_str(),
        "animation": anim,
        "time": time,
        "adapter": renderer.adapter_name(),
    });
    Ok((png, meta))
}
