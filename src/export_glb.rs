//! glTF 2.0 binary writer. ufbx reads FBX and does not write glTF, so this
//! builds a GLB from the in-memory scene. CI checks the result with
//! glTF-Validator.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::error::Result;
use crate::scene::{Scene, Trs};
use crate::MvError;

pub struct ExportStats {
    pub bytes: usize,
    pub meshes: usize,
    pub animations: usize,
}

pub fn export_glb(scene: &Scene, output: &Path, only_anim: Option<&str>) -> Result<ExportStats> {
    let mut bin = Bin::default();
    let mut materials = Vec::new();
    let mut textures = Vec::new();
    let mut images = Vec::new();
    for (i, texture) in scene.textures.iter().enumerate() {
        if texture.rgba.is_empty() || texture.width == 0 || texture.height == 0 {
            continue;
        }
        let png = encode_png(texture.width, texture.height, &texture.rgba)?;
        let view = bin.push_bytes(&png, None);
        images.push(json!({
            "bufferView": view,
            "mimeType": "image/png",
            "name": texture.name,
        }));
        textures.push(json!({
            "source": images.len() - 1,
            "sampler": 0,
            "name": texture.name,
        }));
        let _ = i;
    }
    // textures vec index is not the scene texture index. Map scene texture -> gltf texture.
    let mut tex_map = vec![None; scene.textures.len()];
    let mut gltf_tex = 0usize;
    for (i, texture) in scene.textures.iter().enumerate() {
        if texture.rgba.is_empty() || texture.width == 0 || texture.height == 0 {
            continue;
        }
        tex_map[i] = Some(gltf_tex);
        gltf_tex += 1;
    }

    for material in &scene.materials {
        let mut pbr = json!({
            "baseColorFactor": material.base_color,
            "metallicFactor": material.metallic,
            "roughnessFactor": material.roughness,
        });
        if let Some(tex) = material
            .base_color_texture
            .and_then(|i| tex_map.get(i).copied().flatten())
        {
            pbr["baseColorTexture"] = json!({ "index": tex });
        }
        materials.push(json!({
            "name": material.name,
            "doubleSided": material.double_sided,
            "pbrMetallicRoughness": pbr,
        }));
    }

    let mut meshes = Vec::new();
    let mut mesh_skins: Vec<Option<usize>> = Vec::new();
    for mesh in &scene.meshes {
        if mesh.positions.is_empty() || mesh.indices.is_empty() {
            meshes.push(json!({ "primitives": [] }));
            mesh_skins.push(mesh.skin);
            continue;
        }
        let position_view = bin.push_vec3(&mesh.positions, Some(34962));
        let (min, max) = bounds(&mesh.positions);
        let position_acc = bin.push_accessor(
            position_view,
            mesh.positions.len(),
            "VEC3",
            5126,
            Some(min),
            Some(max),
        );
        let mut attributes = json!({ "POSITION": position_acc });
        if mesh.normals.len() == mesh.positions.len() {
            let view = bin.push_vec3(&mesh.normals, Some(34962));
            attributes["NORMAL"] =
                json!(bin.push_accessor(view, mesh.normals.len(), "VEC3", 5126, None, None));
        }
        for (set_i, set) in mesh.uvs.iter().enumerate().take(4) {
            if set.coords.len() != mesh.positions.len() {
                continue;
            }
            let view = bin.push_vec2(&set.coords, Some(34962));
            attributes[format!("TEXCOORD_{set_i}")] =
                json!(bin.push_accessor(view, set.coords.len(), "VEC2", 5126, None, None));
        }
        if mesh.joints.len() == mesh.positions.len() && mesh.weights.len() == mesh.positions.len() {
            let view = bin.push_u16x4(&mesh.joints, Some(34962));
            attributes["JOINTS_0"] =
                json!(bin.push_accessor(view, mesh.joints.len(), "VEC4", 5123, None, None));
            let view = bin.push_vec4(&mesh.weights, Some(34962));
            attributes["WEIGHTS_0"] =
                json!(bin.push_accessor(view, mesh.weights.len(), "VEC4", 5126, None, None));
        }
        let index_acc = if mesh.positions.len() <= u16::MAX as usize {
            let shorts: Vec<u16> = mesh.indices.iter().map(|i| *i as u16).collect();
            let view = bin.push_u16(&shorts, Some(34963));
            bin.push_accessor(view, shorts.len(), "SCALAR", 5123, None, None)
        } else {
            let view = bin.push_u32(&mesh.indices, Some(34963));
            bin.push_accessor(view, mesh.indices.len(), "SCALAR", 5125, None, None)
        };
        let mut primitive = json!({
            "attributes": attributes,
            "indices": index_acc,
            "mode": 4,
        });
        if let Some(mat) = mesh.material {
            if mat < materials.len() {
                primitive["material"] = json!(mat);
            }
        }
        meshes.push(json!({
            "name": mesh.name,
            "primitives": [primitive],
        }));
        mesh_skins.push(mesh.skin);
    }

    // One glTF mesh per scene mesh. Nodes that reference several meshes get
    // extra child nodes so each glTF node still has a single mesh.
    let mut nodes = Vec::new();
    let mut extra_children: Vec<Vec<usize>> = vec![Vec::new(); scene.nodes.len()];
    let mut node_mesh: Vec<Option<usize>> = vec![None; scene.nodes.len()];
    let mut node_skin: Vec<Option<usize>> = vec![None; scene.nodes.len()];
    let mut next_extra = scene.nodes.len();
    let mut extras = Vec::new();
    for (ni, node) in scene.nodes.iter().enumerate() {
        if let Some(&first) = node.meshes.first() {
            node_mesh[ni] = Some(first);
            node_skin[ni] = mesh_skins.get(first).copied().flatten();
            for &extra_mesh in node.meshes.iter().skip(1) {
                extra_children[ni].push(next_extra);
                extras.push((ni, extra_mesh));
                next_extra += 1;
            }
        }
    }
    for (ni, node) in scene.nodes.iter().enumerate() {
        let mut value = json!({ "name": node.name });
        write_trs(&mut value, node.rest);
        let mut children = node.children.clone();
        children.extend(extra_children[ni].iter().copied());
        if !children.is_empty() {
            value["children"] = json!(children);
        }
        if let Some(mesh) = node_mesh[ni] {
            value["mesh"] = json!(mesh);
        }
        if let Some(skin) = node_skin[ni] {
            value["skin"] = json!(skin);
        }
        nodes.push(value);
    }
    for (parent, mesh_index) in extras {
        let mut value = json!({
            "name": format!("{} / {}", scene.nodes[parent].name, scene.meshes.get(mesh_index).map(|m| m.name.as_str()).unwrap_or("mesh")),
            "mesh": mesh_index,
        });
        if let Some(skin) = mesh_skins.get(mesh_index).copied().flatten() {
            value["skin"] = json!(skin);
        }
        nodes.push(value);
    }

    let mut skins = Vec::new();
    for skin in &scene.skins {
        if skin.joints.is_empty() {
            continue;
        }
        let mats: Vec<f32> = skin
            .inverse_bind
            .iter()
            .flat_map(|m| m.iter().copied())
            .collect();
        let view = bin.push_f32(&mats, None);
        let acc = bin.push_accessor(view, skin.joints.len(), "MAT4", 5126, None, None);
        skins.push(json!({
            "name": skin.name,
            "joints": skin.joints,
            "inverseBindMatrices": acc,
        }));
    }

    let mut animations = Vec::new();
    for anim in &scene.animations {
        if let Some(only) = only_anim {
            if !anim.name.eq_ignore_ascii_case(only) {
                continue;
            }
        }
        if anim.tracks.is_empty() {
            continue;
        }
        let mut samplers = Vec::new();
        let mut channels = Vec::new();
        for track in &anim.tracks {
            if track.times.len() < 2 {
                continue;
            }
            let time_view = bin.push_f32(&track.times, None);
            let time_acc = bin.push_accessor(
                time_view,
                track.times.len(),
                "SCALAR",
                5126,
                Some(json!([track.times[0]])),
                Some(json!([*track.times.last().unwrap_or(&0.0)])),
            );
            let t_view = bin.push_vec3(&track.translation, None);
            let t_acc =
                bin.push_accessor(t_view, track.translation.len(), "VEC3", 5126, None, None);
            push_sampler(
                &mut samplers,
                &mut channels,
                track.node,
                time_acc,
                t_acc,
                "translation",
            );
            let r_view = bin.push_vec4(&track.rotation, None);
            let r_acc = bin.push_accessor(r_view, track.rotation.len(), "VEC4", 5126, None, None);
            push_sampler(
                &mut samplers,
                &mut channels,
                track.node,
                time_acc,
                r_acc,
                "rotation",
            );
            let s_view = bin.push_vec3(&track.scale, None);
            let s_acc = bin.push_accessor(s_view, track.scale.len(), "VEC3", 5126, None, None);
            push_sampler(
                &mut samplers,
                &mut channels,
                track.node,
                time_acc,
                s_acc,
                "scale",
            );
        }
        if channels.is_empty() {
            continue;
        }
        animations.push(json!({
            "name": anim.name,
            "samplers": samplers,
            "channels": channels,
        }));
    }

    let roots: Vec<usize> = scene
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.parent.is_none().then_some(i))
        .collect();
    let mut root = json!({
        "asset": { "version": "2.0", "generator": "modelview 0.1.0" },
        "scene": 0,
        "scenes": [{ "nodes": roots }],
        "nodes": nodes,
        "meshes": meshes,
        "buffers": [{ "byteLength": bin.data.len() }],
        "bufferViews": bin.views,
        "accessors": bin.accessors,
    });
    if !materials.is_empty() {
        root["materials"] = json!(materials);
    }
    if !textures.is_empty() {
        root["textures"] = json!(textures);
        root["images"] = json!(images);
        root["samplers"] =
            json!([{ "magFilter": 9729, "minFilter": 9729, "wrapS": 10497, "wrapT": 10497 }]);
    }
    if !skins.is_empty() {
        root["skins"] = json!(skins);
    }
    if !animations.is_empty() {
        root["animations"] = json!(animations);
    }
    if bin.data.is_empty() {
        root.as_object_mut().unwrap().remove("buffers");
        root.as_object_mut().unwrap().remove("bufferViews");
    } else {
        root["buffers"] = json!([{ "byteLength": bin.data.len() }]);
    }

    let bytes = pack_glb(&root, &bin.data)?;
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(MvError::io)?;
        }
    }
    fs::write(output, &bytes).map_err(MvError::io)?;
    Ok(ExportStats {
        bytes: bytes.len(),
        meshes: meshes.len(),
        animations: animations.len(),
    })
}

fn push_sampler(
    samplers: &mut Vec<Value>,
    channels: &mut Vec<Value>,
    node: usize,
    time_acc: usize,
    output: usize,
    path: &str,
) {
    let sampler = samplers.len();
    samplers.push(json!({
        "input": time_acc,
        "output": output,
        "interpolation": "LINEAR",
    }));
    channels.push(json!({
        "sampler": sampler,
        "target": { "node": node, "path": path },
    }));
}

fn write_trs(node: &mut Value, trs: Trs) {
    if trs.translation.iter().any(|v| v.abs() > 1e-8) {
        node["translation"] = json!(trs.translation);
    }
    let identity_rot = (trs.rotation[3].abs() - 1.0).abs() < 1e-5
        && trs.rotation[0].abs() < 1e-5
        && trs.rotation[1].abs() < 1e-5
        && trs.rotation[2].abs() < 1e-5;
    if !identity_rot {
        node["rotation"] = json!(trs.rotation);
    }
    if trs.scale.iter().any(|v| (v - 1.0).abs() > 1e-5) {
        node["scale"] = json!(trs.scale);
    }
}

fn bounds(positions: &[[f32; 3]]) -> (Value, Value) {
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for p in positions {
        for k in 0..3 {
            min[k] = min[k].min(p[k]);
            max[k] = max[k].max(p[k]);
        }
    }
    (json!(min), json!(max))
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| MvError::export("texture bytes do not match its dimensions"))?;
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|err| MvError::export(err.to_string()))?;
    Ok(bytes)
}

fn pack_glb(root: &Value, bin: &[u8]) -> Result<Vec<u8>> {
    let mut json_bytes =
        serde_json::to_vec(root).map_err(|err| MvError::export(err.to_string()))?;
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }
    let mut bin_bytes = bin.to_vec();
    while bin_bytes.len() % 4 != 0 {
        bin_bytes.push(0);
    }
    let total = 12
        + 8
        + json_bytes.len()
        + if bin_bytes.is_empty() {
            0
        } else {
            8 + bin_bytes.len()
        };
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0x46546C67u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F534Au32.to_le_bytes());
    out.extend(json_bytes);
    if !bin_bytes.is_empty() {
        out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x004E4942u32.to_le_bytes());
        out.extend(bin_bytes);
    }
    Ok(out)
}

#[derive(Default)]
struct Bin {
    data: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Bin {
    fn align(&mut self) {
        while self.data.len() % 4 != 0 {
            self.data.push(0);
        }
    }

    fn push_bytes(&mut self, bytes: &[u8], target: Option<u32>) -> usize {
        self.align();
        let offset = self.data.len();
        self.data.extend_from_slice(bytes);
        let mut view = json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": bytes.len(),
        });
        if let Some(target) = target {
            view["target"] = json!(target);
        }
        self.views.push(view);
        self.views.len() - 1
    }

    fn push_f32(&mut self, values: &[f32], target: Option<u32>) -> usize {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.push_bytes(&bytes, target)
    }

    fn push_vec3(&mut self, values: &[[f32; 3]], target: Option<u32>) -> usize {
        let flat: Vec<f32> = values.iter().flat_map(|v| v.iter().copied()).collect();
        self.push_f32(&flat, target)
    }

    fn push_vec2(&mut self, values: &[[f32; 2]], target: Option<u32>) -> usize {
        let flat: Vec<f32> = values.iter().flat_map(|v| v.iter().copied()).collect();
        self.push_f32(&flat, target)
    }

    fn push_vec4(&mut self, values: &[[f32; 4]], target: Option<u32>) -> usize {
        let flat: Vec<f32> = values.iter().flat_map(|v| v.iter().copied()).collect();
        self.push_f32(&flat, target)
    }

    fn push_u16(&mut self, values: &[u16], target: Option<u32>) -> usize {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.push_bytes(&bytes, target)
    }

    fn push_u16x4(&mut self, values: &[[u16; 4]], target: Option<u32>) -> usize {
        let flat: Vec<u16> = values.iter().flat_map(|v| v.iter().copied()).collect();
        self.push_u16(&flat, target)
    }

    fn push_u32(&mut self, values: &[u32], target: Option<u32>) -> usize {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.push_bytes(&bytes, target)
    }

    fn push_accessor(
        &mut self,
        view: usize,
        count: usize,
        ty: &str,
        component: u32,
        min: Option<Value>,
        max: Option<Value>,
    ) -> usize {
        let mut acc = json!({
            "bufferView": view,
            "componentType": component,
            "count": count,
            "type": ty,
        });
        if let Some(min) = min {
            acc["min"] = min;
        }
        if let Some(max) = max {
            acc["max"] = max;
        }
        self.accessors.push(acc);
        self.accessors.len() - 1
    }
}
