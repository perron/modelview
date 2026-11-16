use serde::Serialize;

use crate::scene::{Problem, Scene};
use crate::validate;

#[derive(Serialize)]
struct Report {
    schema: &'static str,
    ok: bool,
    file: FileInfo,
    counts: Counts,
    bounds: Bounds,
    #[serde(skip_serializing_if = "Option::is_none")]
    fbx: Option<FbxInfo>,
    notes: Notes,
    #[serde(skip_serializing_if = "Option::is_none")]
    nodes: Option<Vec<NodeInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meshes: Option<Vec<MeshInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    materials: Option<Vec<MaterialInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    textures: Option<Vec<TextureInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    animations: Option<Vec<AnimInfo>>,
    problems: Vec<Problem>,
}

#[derive(Serialize)]
struct FileInfo {
    path: String,
    name: String,
    format: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Serialize)]
struct FbxInfo {
    version: u32,
    ascii: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_unit_meters: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_up_axis: Option<String>,
}

#[derive(Serialize)]
struct Counts {
    nodes: usize,
    meshes: usize,
    vertices: usize,
    triangles: usize,
    materials: usize,
    textures: usize,
    bones: usize,
    skins: usize,
    animations: usize,
}

#[derive(Serialize)]
struct Bounds {
    min: [f32; 3],
    max: [f32; 3],
    center: [f32; 3],
    radius: f32,
}

#[derive(Serialize)]
struct Notes {
    units: String,
    axes: String,
    skinning: &'static str,
}

#[derive(Serialize)]
struct NodeInfo {
    name: String,
    parent: Option<usize>,
    mesh_count: usize,
    bone: bool,
}

#[derive(Serialize)]
struct MeshInfo {
    name: String,
    vertices: usize,
    triangles: usize,
    uv_sets: Vec<String>,
    skinned: bool,
    max_influences: usize,
    material: Option<String>,
}

#[derive(Serialize)]
struct MaterialInfo {
    name: String,
    shading_model: String,
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    base_color_texture: Option<String>,
}

#[derive(Serialize)]
struct TextureInfo {
    name: String,
    referenced_path: String,
    resolved_path: Option<String>,
    embedded: bool,
    status: String,
    width: u32,
    height: u32,
    searched: Vec<String>,
}

#[derive(Serialize)]
struct AnimInfo {
    name: String,
    duration: f32,
    fps: f32,
    tracks: usize,
}

pub fn info_json(scene: &Scene, detail: &str) -> serde_json::Value {
    let want = |name: &str| detail == "all" || detail.split(',').any(|p| p.trim() == name);
    let report = Report {
        schema: "modelview/info/v1",
        ok: true,
        file: FileInfo {
            path: scene.source.path.clone(),
            name: scene.source.file_name.clone(),
            format: scene.source.format.clone(),
            size_bytes: scene.source.size_bytes,
            sha256: scene.source.sha256.clone(),
        },
        fbx: scene.source.fbx_version.map(|version| FbxInfo {
            version,
            ascii: scene.source.fbx_ascii.unwrap_or(false),
            creator: scene.source.creator.clone(),
            original_unit_meters: scene.source.original_unit_meters,
            original_up_axis: scene.source.original_up_axis.clone(),
        }),
        counts: Counts {
            nodes: scene.nodes.len(),
            meshes: scene.meshes.len(),
            vertices: scene.vertex_count(),
            triangles: scene.triangle_count(),
            materials: scene.materials.len(),
            textures: scene.textures.len(),
            bones: scene.bone_count(),
            skins: scene.skins.len(),
            animations: scene.animations.len(),
        },
        bounds: Bounds {
            min: scene.bounds.min,
            max: scene.bounds.max,
            center: scene.bounds.center(),
            radius: scene.bounds.radius(),
        },
        notes: Notes {
            units: "Displayed in meters. The file's original unit is in `fbx.original_unit_meters` when the source is FBX.".into(),
            axes: "Displayed Y-up, right-handed, with +Z as the front preset.".into(),
            skinning: "Linear blend, at most 4 influences, evaluated on the CPU.",
        },
        nodes: want("nodes").then(|| {
            scene.nodes
                .iter()
                .map(|n| NodeInfo {
                    name: n.name.clone(),
                    parent: n.parent,
                    mesh_count: n.meshes.len(),
                    bone: n.is_bone,
                })
                .collect()
        }),
        meshes: want("meshes").then(|| {
            scene.meshes
                .iter()
                .map(|m| MeshInfo {
                    name: m.name.clone(),
                    vertices: m.positions.len(),
                    triangles: m.indices.len() / 3,
                    uv_sets: m.uvs.iter().map(|s| s.name.clone()).collect(),
                    skinned: m.skin.is_some(),
                    max_influences: m.max_influences(),
                    material: m.material.and_then(|i| scene.materials.get(i)).map(|mat| mat.name.clone()),
                })
                .collect()
        }),
        materials: want("materials").then(|| {
            scene.materials
                .iter()
                .map(|m| MaterialInfo {
                    name: m.name.clone(),
                    shading_model: m.shading_model.clone(),
                    base_color: m.base_color,
                    metallic: m.metallic,
                    roughness: m.roughness,
                    base_color_texture: m.base_color_texture.and_then(|i| scene.textures.get(i)).map(|t| t.name.clone()),
                })
                .collect()
        }),
        textures: want("textures").then(|| {
            scene.textures
                .iter()
                .map(|t| TextureInfo {
                    name: t.name.clone(),
                    referenced_path: t.referenced_path.clone(),
                    resolved_path: t.resolved_path.clone(),
                    embedded: t.embedded,
                    status: t.status.clone(),
                    width: t.width,
                    height: t.height,
                    searched: t.searched.clone(),
                })
                .collect()
        }),
        animations: want("anims").then(|| {
            scene.animations
                .iter()
                .map(|a| AnimInfo {
                    name: a.name.clone(),
                    duration: a.duration,
                    fps: a.fps,
                    tracks: a.tracks.len(),
                })
                .collect()
        }),
        problems: scene.problems.clone(),
    };
    serde_json::to_value(report)
        .unwrap_or(serde_json::json!({"schema": "modelview/error/v1", "ok": false}))
}

pub fn validate_json(scene: &Scene) -> serde_json::Value {
    let (errors, warnings, infos) = validate::counts(scene);
    serde_json::json!({
        "schema": "modelview/validate/v1",
        "ok": errors == 0,
        "file": scene.source.path,
        "error_count": errors,
        "warning_count": warnings,
        "info_count": infos,
        "problems": scene.problems,
    })
}

pub fn animations_json(scene: &Scene) -> serde_json::Value {
    serde_json::json!({
        "schema": "modelview/animations/v1",
        "ok": true,
        "file": scene.source.path,
        "animations": scene.animations.iter().map(|a| serde_json::json!({
            "name": a.name,
            "duration": a.duration,
            "fps": a.fps,
            "tracks": a.tracks.len(),
        })).collect::<Vec<_>>(),
    })
}

pub fn human_info(scene: &Scene) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}  {}  {} bytes  sha256 {}\n",
        scene.source.file_name, scene.source.format, scene.source.size_bytes, scene.source.sha256
    ));
    if let Some(version) = scene.source.fbx_version {
        out.push_str(&format!(
            "fbx version {version} {}\n",
            if scene.source.fbx_ascii.unwrap_or(false) {
                "ascii"
            } else {
                "binary"
            }
        ));
    }
    out.push_str(&format!(
        "nodes {}  meshes {}  vertices {}  triangles {}\n",
        scene.nodes.len(),
        scene.meshes.len(),
        scene.vertex_count(),
        scene.triangle_count()
    ));
    out.push_str(&format!(
        "materials {}  textures {}  bones {}  skins {}  animations {}\n",
        scene.materials.len(),
        scene.textures.len(),
        scene.bone_count(),
        scene.skins.len(),
        scene.animations.len()
    ));
    for anim in &scene.animations {
        out.push_str(&format!(
            "  take '{}'  {:.3}s  {:.1} fps\n",
            anim.name, anim.duration, anim.fps
        ));
    }
    if scene.problems.is_empty() {
        out.push_str("problems: none\n");
    } else {
        out.push_str("problems:\n");
        for problem in &scene.problems {
            out.push_str(&format!(
                "  {} {}  {}\n",
                problem.severity, problem.code, problem.message
            ));
            if !problem.searched.is_empty() {
                out.push_str("    searched:\n");
                for path in &problem.searched {
                    out.push_str(&format!("      {path}\n"));
                }
            }
        }
    }
    out
}

pub fn human_validate(scene: &Scene) -> String {
    let (errors, warnings, infos) = validate::counts(scene);
    let mut out = human_info(scene);
    out.push_str(&format!(
        "summary: {errors} errors, {warnings} warnings, {infos} info\n"
    ));
    out
}
