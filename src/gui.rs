use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crate::load::{self, load_isolated};
use crate::render::{FrameRequest, RenderMode, Renderer};
use crate::scene::Scene;
use crate::MvError;

pub fn run(file: Option<PathBuf>) -> i32 {
    // Installed before the window exists, so a signal during the first frame
    // is remembered. Loading a file used to be the only place this ran, which
    // left an idle window swallowing SIGTERM and SIGINT.
    load::install_stop_handler();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("modelview")
            .with_icon(window_icon()),
        ..Default::default()
    };
    match eframe::run_native(
        "modelview",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            std::thread::Builder::new()
                .name("modelview-stop".into())
                .spawn(move || {
                    load::block_until_stop();
                    ctx.request_repaint();
                })
                .ok();
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(Viewer::new(file)))
        }),
    ) {
        Ok(()) => {
            // A quit during a Desktop permission prompt must not leave the
            // temp directory the loader created before the read returned.
            // A signal quit uses the same cleanup and reports 130.
            crate::load::cleanup_active_temp();
            if crate::load::stop_requested() {
                130
            } else {
                0
            }
        }
        Err(err) => {
            eprintln!("modelview: {err}");
            2
        }
    }
}

struct Viewer {
    scene: Option<Scene>,
    error: Option<MvError>,
    loading: Option<String>,
    inbox: Option<Receiver<Result<Scene, MvError>>>,
    renderer: Option<Renderer>,
    renderer_error: Option<String>,
    yaw: f32,
    pitch: f32,
    mode: RenderMode,
    uv_set: usize,
    bone: Option<usize>,
    show_skeleton: bool,
    zoom: f32,
    target_offset: [f32; 3],
    anim: Option<usize>,
    time: f32,
    playing: bool,
    selected: Option<usize>,
    recent: Vec<PathBuf>,
    view_image: Option<egui::TextureHandle>,
    cache_key: u64,
    /// Path waiting until the interface has been drawn. File I/O starts only
    /// after that, so a macOS privacy prompt cannot block the first frame.
    pending: Option<PathBuf>,
    interface_drawn: bool,
}

impl Viewer {
    fn new(file: Option<PathBuf>) -> Self {
        let viewer = Self {
            scene: None,
            error: None,
            loading: None,
            inbox: None,
            renderer: None,
            renderer_error: None,
            yaw: 35.0,
            pitch: 25.0,
            mode: RenderMode::Shaded,
            uv_set: 0,
            bone: None,
            show_skeleton: true,
            zoom: 1.0,
            target_offset: [0.0, 0.0, 0.0],
            anim: None,
            time: 0.0,
            playing: false,
            selected: None,
            recent: Vec::new(),
            view_image: None,
            cache_key: 0,
            pending: file,
            interface_drawn: false,
        };
        viewer
    }

    fn start_load(&mut self, path: PathBuf) {
        self.loading = Some(path.display().to_string());
        self.error = None;
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(8);
        let (tx, rx) = mpsc::channel();
        self.inbox = Some(rx);
        std::thread::spawn(move || {
            let result = load_isolated(&path, Duration::from_secs(60), load::default_memory_mb());
            let _ = tx.send(result);
        });
    }

    fn poll_load(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.inbox.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(scene)) => {
                self.zoom = 1.0;
                self.target_offset = [0.0, 0.0, 0.0];
                self.show_skeleton = scene.bone_count() > 0;
                self.bone = scene.nodes.iter().position(|n| n.is_bone);
                self.anim = None;
                self.time = 0.0;
                self.playing = false;
                self.selected = None;
                self.yaw = 35.0;
                self.pitch = 25.0;
                self.scene = Some(scene);
                self.loading = None;
                self.inbox = None;
                self.cache_key = self.cache_key.wrapping_add(1);
            }
            Ok(Err(err)) => {
                self.error = Some(err);
                self.scene = None;
                self.loading = None;
                self.inbox = None;
            }
            Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.error = Some(MvError::parse("loader thread stopped"));
                self.loading = None;
                self.inbox = None;
            }
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("3D models", &["fbx", "gltf", "glb", "obj", "stl", "ply"])
            .pick_file()
        {
            self.pending = Some(path);
        }
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if load::stop_requested() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.poll_load(ui.ctx());
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if let Some(path) = dropped.into_iter().next() {
            self.pending = Some(path);
        }
        if ui.input(|i| i.key_pressed(egui::Key::F)) {
            self.yaw = 35.0;
            self.pitch = 25.0;
        }

        ui.vertical(|ui| {
            self.toolbar(ui);
            let available = ui.available_height();
            let bottom = 168.0_f32.min(available * 0.35);
            let body = (available - bottom).max(120.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), body),
                egui::Layout::left_to_right(egui::Align::Min),
                |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(240.0, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.outliner(ui),
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(
                            (ui.available_width() - 280.0).max(160.0),
                            ui.available_height(),
                        ),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.viewport(ui),
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.inspector(ui),
                    );
                },
            );
            self.timeline(ui);
            self.problems(ui);
        });
        // The widgets above are already recorded. Starting the read on the
        // next frame lets this one present before a Desktop open() can stall.
        if io_allowed_this_frame(&mut self.interface_drawn) {
            if let Some(path) = self.pending.take() {
                self.start_load(path);
            }
        } else {
            ui.ctx().request_repaint();
        }
    }
}

/// File reads wait until a frame has been drawn. The first call returns false
/// and arms the gate; later calls return true.
pub(crate) fn io_allowed_this_frame(interface_drawn: &mut bool) -> bool {
    if *interface_drawn {
        true
    } else {
        *interface_drawn = true;
        false
    }
}

impl Viewer {
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open...").clicked() {
                    self.open_dialog();
                }
                ui.separator();
                for path in self.recent.clone() {
                    let label = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    if ui.button(label).clicked() {
                        self.pending = Some(path);
                    }
                }
            });
            if ui.button("Open").clicked() {
                self.open_dialog();
            }
            ui.separator();
            for (label, yaw, pitch) in [
                ("Front", 0.0, 0.0),
                ("Back", 180.0, 0.0),
                ("Left", -90.0, 0.0),
                ("Right", 90.0, 0.0),
                ("Top", 0.0, 89.0),
                ("3/4", 35.0, 25.0),
            ] {
                if ui.button(label).clicked() {
                    self.yaw = yaw;
                    self.pitch = pitch;
                }
            }
            ui.separator();
            for (label, mode) in [
                ("Shaded", RenderMode::Shaded),
                ("Wire", RenderMode::Wireframe),
                ("Normals", RenderMode::Normals),
                ("UV", RenderMode::UvChecker),
                ("Weights", RenderMode::Weights),
            ] {
                if ui.selectable_label(self.mode == mode, label).clicked() {
                    self.mode = mode;
                }
            }
            ui.separator();
            ui.checkbox(&mut self.show_skeleton, "Skeleton");
            if let Some(scene) = &self.scene {
                let names: Vec<String> = scene
                    .meshes
                    .iter()
                    .flat_map(|m| m.uvs.iter().map(|s| s.name.clone()))
                    .collect();
                let unique = unique_names(&names);
                if !unique.is_empty() {
                    let current = unique
                        .get(self.uv_set)
                        .cloned()
                        .unwrap_or_else(|| unique[0].clone());
                    egui::ComboBox::from_label("UV")
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for (i, name) in unique.iter().enumerate() {
                                ui.selectable_value(&mut self.uv_set, i, name);
                            }
                        });
                }
            }
        });
        if let Some(path) = &self.loading {
            ui.label(format!("Loading {path}"));
        } else if let Some(path) = &self.pending {
            ui.label(format!("Loading {}", path.display()));
        }
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        let size = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        if response.dragged_by(egui::PointerButton::Primary) {
            let d = response.drag_delta();
            self.yaw -= d.x * 0.4;
            self.pitch = (self.pitch + d.y * 0.4).clamp(-89.0, 89.0);
        }
        if response.dragged_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
        {
            let d = response.drag_delta();
            let radius = self
                .scene
                .as_ref()
                .map(|s| s.bounds.radius())
                .unwrap_or(1.0);
            let scale = 0.0025 * radius * self.zoom;
            let yaw = self.yaw.to_radians();
            let right_x = yaw.cos();
            let right_z = -yaw.sin();
            self.target_offset[0] -= d.x * scale * right_x;
            self.target_offset[2] -= d.x * scale * right_z;
            self.target_offset[1] += d.y * scale;
        }
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if response.hovered() && scroll.abs() > 0.0 {
            self.zoom = (self.zoom * (1.0 - scroll * 0.001)).clamp(0.05, 20.0);
        }

        if let Some(err) = &self.error {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                err.to_string(),
                egui::FontId::proportional(16.0),
                egui::Color32::LIGHT_RED,
            );
            return;
        }
        let Some(scene) = self.scene.clone() else {
            let message = if let Some(path) = self.loading.as_ref() {
                format!("Loading {path}")
            } else if let Some(path) = self.pending.as_ref() {
                format!("Loading {}", path.display())
            } else {
                "Open a model, or drop one here".into()
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                message,
                egui::FontId::proportional(18.0),
                egui::Color32::GRAY,
            );
            return;
        };
        if self.playing {
            if let Some(anim) = self.anim.and_then(|i| scene.animations.get(i)) {
                let dt = ui.input(|i| i.stable_dt);
                self.time += dt;
                if anim.duration > 0.0 && self.time > anim.duration {
                    self.time = 0.0;
                }
                ui.ctx().request_repaint();
            }
        }
        let ppp = ui.ctx().pixels_per_point();
        let mut w = (rect.width() * ppp).round() as u32;
        let mut h = (rect.height() * ppp).round() as u32;
        w = w.clamp(16, 1600);
        h = h.clamp(16, 1200);
        let key = cache_key(
            w,
            h,
            self.yaw,
            self.pitch,
            self.zoom,
            self.target_offset,
            self.mode,
            self.uv_set,
            self.bone,
            self.anim,
            self.time,
            self.show_skeleton,
        );
        if self.cache_key != key || self.view_image.is_none() {
            self.cache_key = key;
            if self.renderer.is_none() && self.renderer_error.is_none() {
                match Renderer::new() {
                    Ok(renderer) => self.renderer = Some(renderer),
                    Err(err) => self.renderer_error = Some(err.to_string()),
                }
            }
            if let Some(renderer) = self.renderer.as_mut() {
                match renderer.render_rgba(&FrameRequest {
                    scene: &scene,
                    width: w,
                    height: h,
                    yaw: self.yaw,
                    pitch: self.pitch,
                    mode: self.mode,
                    anim: self.anim,
                    time: self.time,
                    uv_set: self.uv_set,
                    bone: self.bone,
                    show_skeleton: self.show_skeleton,
                    show_grid: true,
                    show_surface: true,
                    distance_scale: self.zoom,
                    target_offset: self.target_offset,
                }) {
                    Ok(image) => {
                        let color = egui::ColorImage::from_rgba_unmultiplied(
                            [image.width() as usize, image.height() as usize],
                            image.as_raw(),
                        );
                        self.view_image = Some(ui.ctx().load_texture(
                            "viewport",
                            color,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.renderer_error = None;
                    }
                    Err(err) => self.renderer_error = Some(err.to_string()),
                }
            }
        }
        if let Some(tex) = &self.view_image {
            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else if let Some(err) = &self.renderer_error {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                err,
                egui::FontId::proportional(14.0),
                egui::Color32::LIGHT_RED,
            );
        }
    }

    fn outliner(&mut self, ui: &mut egui::Ui) {
        ui.heading("Outliner");
        egui::ScrollArea::vertical().show(ui, |ui| {
            let Some(scene) = &self.scene else {
                ui.label("No model");
                return;
            };
            let roots: Vec<usize> = scene
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(i, n)| n.parent.is_none().then_some(i))
                .collect();
            let mut selected = self.selected;
            for root in roots {
                draw_node(ui, scene, root, 0, &mut selected);
            }
            self.selected = selected;
            if let Some(index) = selected {
                if scene.nodes.get(index).map(|n| n.is_bone).unwrap_or(false) {
                    self.bone = Some(index);
                }
            }
        });
    }

    fn inspector(&self, ui: &mut egui::Ui) {
        ui.heading("Inspector");
        egui::ScrollArea::vertical().show(ui, |ui| {
            let Some(scene) = &self.scene else {
                ui.label("Nothing selected");
                return;
            };
            ui.label(format!("file  {}", scene.source.file_name));
            ui.label(format!("format  {}", scene.source.format));
            if let Some(version) = scene.source.fbx_version {
                ui.label(format!(
                    "fbx  {version} {}",
                    if scene.source.fbx_ascii.unwrap_or(false) {
                        "ASCII"
                    } else {
                        "binary"
                    }
                ));
            }
            if let Some(unit) = scene.source.original_unit_meters {
                ui.label(format!("file unit  {unit} m"));
            }
            if let Some(up) = &scene.source.original_up_axis {
                ui.label(format!("file up axis  {up}"));
            }
            ui.separator();
            ui.label(format!("triangles  {}", scene.triangle_count()));
            ui.label(format!("vertices  {}", scene.vertex_count()));
            ui.label(format!("nodes  {}", scene.nodes.len()));
            ui.label(format!("bones  {}", scene.bone_count()));
            ui.label(format!("materials  {}", scene.materials.len()));
            ui.label(format!("textures  {}", scene.textures.len()));
            ui.separator();
            if let Some(index) = self.selected.and_then(|i| scene.nodes.get(i).map(|_| i)) {
                let node = &scene.nodes[index];
                ui.strong(&node.name);
                ui.label(if node.is_bone { "bone" } else { "node" });
                ui.label(format!(
                    "translate  {:.3} {:.3} {:.3}",
                    node.rest.translation[0], node.rest.translation[1], node.rest.translation[2]
                ));
                ui.label(format!(
                    "scale  {:.3} {:.3} {:.3}",
                    node.rest.scale[0], node.rest.scale[1], node.rest.scale[2]
                ));
                for &mi in &node.meshes {
                    if let Some(mesh) = scene.meshes.get(mi) {
                        ui.separator();
                        ui.label(format!("mesh  {}", mesh.name));
                        ui.label(format!(
                            "verts {}  tris {}",
                            mesh.positions.len(),
                            mesh.indices.len() / 3
                        ));
                        if !mesh.uvs.is_empty() {
                            ui.label(format!(
                                "uv  {}",
                                mesh.uvs
                                    .iter()
                                    .map(|s| s.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                        }
                        if mesh.skin.is_some() {
                            ui.label(format!("influences  {}", mesh.max_influences()));
                        }
                        if let Some(mat) = mesh.material.and_then(|i| scene.materials.get(i)) {
                            ui.label(format!("material  {}", mat.name));
                            ui.label(format!("shading  {}", mat.shading_model));
                            ui.label(format!(
                                "color  {:.2} {:.2} {:.2}  metal {:.2}  rough {:.2}",
                                mat.base_color[0],
                                mat.base_color[1],
                                mat.base_color[2],
                                mat.metallic,
                                mat.roughness
                            ));
                        }
                    }
                }
            }
            if !scene.textures.is_empty() {
                ui.separator();
                ui.strong("Textures");
                for texture in &scene.textures {
                    ui.label(format!(
                        "{}  {}  {}",
                        texture.name,
                        texture.status,
                        if texture.embedded {
                            "embedded"
                        } else {
                            "external"
                        }
                    ));
                    if !texture.referenced_path.is_empty() {
                        ui.label(egui::RichText::new(&texture.referenced_path).small());
                    }
                }
            }
        });
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        let Some(scene) = self.scene.clone() else {
            return;
        };
        ui.horizontal(|ui| {
            let names: Vec<(usize, String)> = scene
                .animations
                .iter()
                .enumerate()
                .map(|(i, a)| (i, a.name.clone()))
                .collect();
            if names.is_empty() {
                ui.label("No animation");
                return;
            }
            let current = self
                .anim
                .and_then(|i| {
                    names
                        .iter()
                        .find(|(n, _)| *n == i)
                        .map(|(_, name)| name.clone())
                })
                .unwrap_or_else(|| "take".into());
            egui::ComboBox::from_label("Take")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (i, name) in &names {
                        if ui.selectable_label(self.anim == Some(*i), name).clicked() {
                            self.anim = Some(*i);
                            self.time = 0.0;
                        }
                    }
                });
            let duration = self
                .anim
                .and_then(|i| scene.animations.get(i))
                .map(|a| a.duration)
                .unwrap_or(0.0);
            if ui
                .button(if self.playing { "Pause" } else { "Play" })
                .clicked()
            {
                self.playing = !self.playing;
                if self.anim.is_none() {
                    self.anim = Some(names[0].0);
                }
            }
            let mut time = self.time;
            ui.add(egui::Slider::new(&mut time, 0.0..=duration.max(0.001)).text("sec"));
            self.time = time;
        });
    }

    fn problems(&self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Problems");
        egui::ScrollArea::vertical()
            .max_height(110.0)
            .show(ui, |ui| {
                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err.to_string());
                }
                let Some(scene) = &self.scene else {
                    if self.loading.is_some() || self.pending.is_some() {
                        ui.label("Loading");
                    }
                    return;
                };
                if scene.problems.is_empty() {
                    ui.label("None");
                    return;
                }
                for problem in &scene.problems {
                    let color = match problem.severity.as_str() {
                        "error" => egui::Color32::LIGHT_RED,
                        "warning" => egui::Color32::GOLD,
                        _ => egui::Color32::LIGHT_BLUE,
                    };
                    ui.colored_label(
                        color,
                        format!("{} {}  {}", problem.severity, problem.code, problem.message),
                    );
                    for path in &problem.searched {
                        ui.label(
                            egui::RichText::new(format!("  searched {path}"))
                                .small()
                                .weak(),
                        );
                    }
                }
            });
    }
}

fn draw_node(
    ui: &mut egui::Ui,
    scene: &Scene,
    index: usize,
    depth: usize,
    selected: &mut Option<usize>,
) {
    let Some(node) = scene.nodes.get(index) else {
        return;
    };
    let label = format!(
        "{}{}{}",
        "  ".repeat(depth),
        if node.is_bone { "bone  " } else { "" },
        node.name
    );
    if ui
        .selectable_label(*selected == Some(index), label)
        .clicked()
    {
        *selected = Some(index);
    }
    for &child in &node.children {
        draw_node(ui, scene, child, depth + 1, selected);
    }
}

#[cfg(test)]
mod tests {
    use super::{io_allowed_this_frame, window_icon};

    #[test]
    fn window_icon_is_the_cube_mark() {
        let icon = window_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        let top = &icon.rgba[(96 * 256 + 128) * 4..][..4];
        assert!(top[0] > 180 && top[3] == 255, "light top face at 256 px");
        assert_eq!(&icon.rgba[..4], &[0, 0, 0, 0], "transparent rounded corner");

        let small = image::load_from_memory(include_bytes!("../packaging/icon/icon-16.png"))
            .unwrap()
            .into_rgba8();
        assert_eq!(small.dimensions(), (16, 16));
        let face = small.get_pixel(8, 6);
        assert!(face[0] > 160 && face[3] == 255, "cube still reads at 16 px");
    }

    #[test]
    fn file_read_waits_until_the_interface_has_been_drawn() {
        let mut drawn = false;
        assert!(!io_allowed_this_frame(&mut drawn));
        assert!(drawn);
        assert!(io_allowed_this_frame(&mut drawn));
    }
}

fn window_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../packaging/icon/icon-256.png"))
        .expect("modelview window icon")
        .into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn unique_names(names: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        if !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

fn cache_key(
    w: u32,
    h: u32,
    yaw: f32,
    pitch: f32,
    zoom: f32,
    offset: [f32; 3],
    mode: RenderMode,
    uv: usize,
    bone: Option<usize>,
    anim: Option<usize>,
    time: f32,
    skeleton: bool,
) -> u64 {
    let mut key = (w as u64) << 32 | h as u64;
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((yaw * 100.0) as i32 as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((pitch * 100.0) as i32 as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((zoom * 1000.0) as i32 as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((offset[0] * 1000.0) as i32 as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((offset[1] * 1000.0) as i32 as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((offset[2] * 1000.0) as i32 as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add(mode as u64);
    key = key.wrapping_mul(0x9E3779B185EBCA87).wrapping_add(uv as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add(bone.unwrap_or(9999) as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add(anim.unwrap_or(9999) as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add((time * 1000.0) as u64);
    key = key
        .wrapping_mul(0x9E3779B185EBCA87)
        .wrapping_add(skeleton as u64);
    key
}
