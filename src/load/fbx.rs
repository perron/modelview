use std::collections::HashMap;
use std::fs;
use std::path::Path;

use glam::{Mat4, Vec4};

use crate::error::Result;
use crate::load::textures::{self, decode_bytes, decode_bytes_named};
use crate::scene::{
    push_checked, Animation, Material, Mesh, Node, Problem, Scene, Skin, Texture, Track, Trs, UvSet,
};
use crate::MvError;

pub fn load(path: &Path) -> Result<Scene> {
    let path_str = path
        .to_str()
        .ok_or_else(|| MvError::parse("FBX path is not valid UTF-8"))?;
    let mut opts = ufbx::LoadOpts::default();
    opts.generate_missing_normals = true;
    opts.target_unit_meters = 1.0;
    opts.target_axes = ufbx::CoordinateAxes::right_handed_y_up();
    // Bake the file's unit into the vertices. AdjustTransforms leaves Maya and
    // Unreal centimetre meshes in file units while scaling only the nodes, and
    // the bind translations stay in centimetres, so skinning collapses the mesh.
    // ModifyGeometry is the conversion ufbx documents for a viewer: vertices,
    // bind matrices, and bone translations end up in the same meter space.
    opts.space_conversion = ufbx::SpaceConversion::ModifyGeometry;
    opts.geometry_transform_handling = ufbx::GeometryTransformHandling::ModifyGeometry;
    opts.clean_skin_weights = true;
    opts.ignore_missing_external_files = true;
    opts.load_external_files = false;
    let loaded = ufbx::load_file(path_str, opts)
        .map_err(|err| MvError::parse(format!("ufbx: {}", err.description.as_ref())))?;

    let mut scene = Scene::new("fbx");
    scene.source.fbx_version = Some(loaded.metadata.version);
    scene.source.fbx_ascii = Some(loaded.metadata.ascii);
    let creator = ustr(&loaded.metadata.creator);
    if !creator.is_empty() {
        scene.source.creator = Some(creator);
    }
    let unit = loaded.settings.original_unit_meters;
    if unit.is_finite() && unit > 0.0 {
        scene.source.original_unit_meters = Some(unit as f32);
    }
    scene.source.original_up_axis = Some(axis_name(loaded.settings.original_axis_up).to_string());

    let mut id_to_index: HashMap<u32, usize> = HashMap::new();
    for i in 0..loaded.nodes.len() {
        id_to_index.insert(loaded.nodes[i].element.element_id, i);
    }

    for i in 0..loaded.nodes.len() {
        let node = &loaded.nodes[i];
        let rest = trs_from_matrix(mat_from(node.node_to_parent));
        scene.nodes.push(Node {
            name: element_name(&node.element.name, "node"),
            parent: None,
            children: Vec::new(),
            rest,
            meshes: Vec::new(),
            is_bone: node.bone.is_some(),
        });
    }
    for i in 0..loaded.nodes.len() {
        if let Some(parent) = loaded.nodes[i].parent.as_ref() {
            if let Some(&pi) = id_to_index.get(&parent.element.element_id) {
                if pi != i {
                    scene.nodes[i].parent = Some(pi);
                    scene.nodes[pi].children.push(i);
                }
            }
        }
    }

    let mut tex_ids: HashMap<u32, usize> = HashMap::new();
    for tex in &loaded.textures {
        let idx = push_texture(&mut scene, path, tex);
        tex_ids.insert(tex.element.element_id, idx);
    }

    let mut mat_ids: HashMap<u32, usize> = HashMap::new();
    for mat in &loaded.materials {
        let idx = scene.materials.len();
        scene.materials.push(convert_material(mat, &tex_ids));
        mat_ids.insert(mat.element.element_id, idx);
    }

    let mut mesh_ids: HashMap<u32, Vec<usize>> = HashMap::new();
    for mi in 0..loaded.meshes.len() {
        let produced = convert_mesh(&mut scene, &loaded.meshes[mi], &mat_ids, &id_to_index)?;
        mesh_ids.insert(loaded.meshes[mi].element.element_id, produced);
    }
    for i in 0..loaded.nodes.len() {
        if let Some(mesh) = loaded.nodes[i].mesh.as_ref() {
            if let Some(list) = mesh_ids.get(&mesh.element.element_id) {
                scene.nodes[i].meshes.extend(list.iter().copied());
            }
        }
    }

    for stack in &loaded.anim_stacks {
        if let Some(anim) = convert_anim(&loaded, stack) {
            scene.animations.push(anim);
        }
    }

    note_file(&mut scene, &loaded);
    Ok(scene)
}

fn note_file(scene: &mut Scene, loaded: &ufbx::Scene) {
    let version = loaded.metadata.version;
    if loaded.metadata.ascii {
        scene.add_problem(Problem::info(
            "ASCII_FBX",
            format!("ASCII FBX (version {version})"),
        ));
    }
    if version > 0 && version < 7000 {
        scene.add_problem(Problem::info(
            "OLD_FBX_VERSION",
            format!("FBX version {version} is older than 7000 (7.0). It is supported, and shown here so the age of the file is visible"),
        ));
    }
    if let Some(unit) = scene.source.original_unit_meters {
        let code = if (unit - 0.01).abs() < 1e-4 {
            "UNITS_CM"
        } else if (unit - 0.001).abs() < 1e-6 {
            "UNITS_MM"
        } else if (unit - 1.0).abs() > 1e-3 {
            "UNITS_NOTE"
        } else {
            ""
        };
        if !code.is_empty() {
            scene.add_problem(Problem::info(
                code,
                format!("file unit is {unit} meters per unit. The scene is displayed in meters"),
            ));
        }
    }
    if scene.source.original_up_axis.as_deref() != Some("+Y") {
        let up = scene
            .source
            .original_up_axis
            .clone()
            .unwrap_or_else(|| "unknown".into());
        scene.add_problem(Problem::info(
            "AXIS_NOTE",
            format!("file up axis is {up}. The scene is displayed Y-up"),
        ));
    }
    if loaded.nurbs_curves.len() + loaded.nurbs_surfaces.len() > 0 {
        scene.add_problem(Problem::info(
            "UNSUPPORTED_FEATURE",
            "NURBS curves or surfaces are present and were not tessellated",
        ));
    }
    if loaded.blend_deformers.len() > 0 {
        scene.add_problem(Problem::info(
            "BLEND_SHAPES_NOT_APPLIED",
            format!(
                "{} blend shape deformer(s) were read and are not applied in v0.1",
                loaded.blend_deformers.len()
            ),
        ));
    }
}

fn convert_anim(loaded: &ufbx::Scene, stack: &ufbx::AnimStack) -> Option<Animation> {
    let t0 = stack.time_begin;
    let t1 = stack.time_end;
    if !t0.is_finite() || !t1.is_finite() {
        return None;
    }
    let duration = (t1 - t0).max(0.0);
    let file_fps = loaded.settings.frames_per_second;
    let fps = if file_fps.is_finite() && (1.0..480.0).contains(&file_fps) {
        file_fps
    } else {
        30.0
    };
    let mut samples = if duration < 1e-6 {
        1
    } else {
        ((duration * fps).round() as usize).clamp(1, 720)
    };
    if duration < 1e-6 {
        samples = 1;
    }
    let anim = &*stack.anim;
    let node_count = loaded.nodes.len();
    let mut times = Vec::with_capacity(samples + 1);
    let mut values: Vec<Vec<Trs>> = vec![Vec::with_capacity(samples + 1); node_count];
    for s in 0..=samples {
        let time = if samples == 0 {
            t0
        } else {
            t0 + duration * (s as f64) / (samples as f64)
        };
        times.push((time - t0) as f32);
        for i in 0..node_count {
            let tr = ufbx::evaluate_transform(anim, &loaded.nodes[i], time);
            values[i].push(trs_from_ufbx(tr));
        }
    }
    let mut tracks = Vec::new();
    for i in 0..node_count {
        let rest = trs_from_matrix(mat_from(loaded.nodes[i].node_to_parent));
        if values[i].iter().all(|sample| !trs_differs(*sample, rest)) {
            continue;
        }
        tracks.push(Track {
            node: i,
            times: times.clone(),
            translation: values[i].iter().map(|t| t.translation).collect(),
            rotation: values[i].iter().map(|t| t.rotation).collect(),
            scale: values[i].iter().map(|t| t.scale).collect(),
        });
    }
    Some(Animation {
        name: element_name(&stack.element.name, "take"),
        duration: duration as f32,
        fps: fps as f32,
        tracks,
    })
}

struct Bucket {
    mesh: Mesh,
}

fn convert_mesh(
    scene: &mut Scene,
    mesh: &ufbx::Mesh,
    mat_ids: &HashMap<u32, usize>,
    id_to_index: &HashMap<u32, usize>,
) -> Result<Vec<usize>> {
    let skin = mesh.skin_deformers.first().map(|s| &**s);
    if mesh.skin_deformers.len() > 1 {
        scene.add_problem(Problem::info(
            "UNSUPPORTED_FEATURE",
            format!(
                "mesh '{}' has {} skin deformers; only the first is used",
                ustr(&mesh.element.name),
                mesh.skin_deformers.len()
            ),
        ));
    }
    let skin_index = if let Some(skin) = skin {
        if !matches!(
            skin.skinning_method,
            ufbx::SkinningMethod::Linear | ufbx::SkinningMethod::Rigid
        ) {
            scene.add_problem(Problem::info(
                "UNSUPPORTED_FEATURE",
                format!(
                    "mesh '{}' uses {:?} skinning; it is previewed as linear blend",
                    ustr(&mesh.element.name),
                    skin.skinning_method
                ),
            ));
        }
        if skin.max_weights_per_vertex > 4 {
            scene.add_problem(Problem::warning(
                "MAX_INFLUENCES_GT_4",
                format!(
                    "mesh '{}' has up to {} influences per vertex; weights were kept to 4 and renormalized",
                    ustr(&mesh.element.name),
                    skin.max_weights_per_vertex
                ),
            ));
        }
        Some(push_skin(scene, skin, id_to_index))
    } else {
        None
    };

    let uv_names: Vec<String> = if mesh.uv_sets.is_empty() {
        if mesh.vertex_uv.exists {
            vec!["UVMap".into()]
        } else {
            Vec::new()
        }
    } else {
        mesh.uv_sets
            .iter()
            .enumerate()
            .map(|(i, set)| {
                let name = ustr(&set.name);
                if name.is_empty() {
                    format!("UV{i}")
                } else {
                    name
                }
            })
            .collect()
    };

    let mut buckets: HashMap<u32, Bucket> = HashMap::new();
    let mut tri_ix = Vec::new();
    let mut tri_count = 0usize;
    for (face_i, face) in mesh.faces.iter().copied().enumerate() {
        let n = ufbx::triangulate_face_vec(&mut tri_ix, mesh, face);
        if n == 0 {
            continue;
        }
        let mat_slot = mesh.face_material.get(face_i).copied().unwrap_or(0);
        let bucket = buckets.entry(mat_slot).or_insert_with(|| {
            let mut built = Mesh::new(element_name(&mesh.element.name, "mesh"));
            built.skin = skin_index;
            built.material = mesh
                .materials
                .get(mat_slot as usize)
                .and_then(|m| mat_ids.get(&m.element.element_id).copied());
            for name in &uv_names {
                built.uvs.push(UvSet {
                    name: name.clone(),
                    coords: Vec::new(),
                });
            }
            Bucket { mesh: built }
        });
        for t in 0..n as usize {
            push_checked(&mut tri_count)?;
            let mut corners = [0u32; 3];
            for k in 0..3 {
                let corner = tri_ix[t * 3 + k] as usize;
                let idx = bucket.mesh.positions.len() as u32;
                bucket
                    .mesh
                    .positions
                    .push(v3(&mesh.vertex_position, corner));
                bucket.mesh.normals.push(v3(&mesh.vertex_normal, corner));
                if mesh.uv_sets.is_empty() {
                    if mesh.vertex_uv.exists {
                        if let Some(set) = bucket.mesh.uvs.get_mut(0) {
                            set.coords.push(v2(&mesh.vertex_uv, corner));
                        }
                    }
                } else {
                    for (si, set) in mesh.uv_sets.iter().enumerate() {
                        if let Some(dst) = bucket.mesh.uvs.get_mut(si) {
                            dst.coords.push(v2(&set.vertex_uv, corner));
                        }
                    }
                }
                if let Some(skin) = skin {
                    let (joints, weights) = skin_vertex(skin, mesh, corner);
                    bucket.mesh.joints.push(joints);
                    bucket.mesh.weights.push(weights);
                }
                corners[k] = idx;
            }
            if mesh.reversed_winding {
                corners.swap(1, 2);
            }
            bucket.mesh.indices.extend(corners);
        }
    }

    let mut produced = Vec::new();
    let mut built: Vec<Bucket> = buckets.into_values().collect();
    if built.is_empty() {
        return Ok(produced);
    }
    built.sort_by(|a, b| a.mesh.material.cmp(&b.mesh.material));
    for bucket in built {
        produced.push(scene.meshes.len());
        scene.meshes.push(bucket.mesh);
    }
    Ok(produced)
}

fn skin_vertex(
    skin: &ufbx::SkinDeformer,
    mesh: &ufbx::Mesh,
    corner: usize,
) -> ([u16; 4], [f32; 4]) {
    let logical = mesh.vertex_indices.get(corner).copied().unwrap_or(0) as usize;
    let Some(sv) = skin.vertices.get(logical) else {
        return ([0; 4], [1.0, 0.0, 0.0, 0.0]);
    };
    let n = (sv.num_weights as usize).min(4);
    let mut joints = [0u16; 4];
    let mut weights = [0.0f32; 4];
    let mut sum = 0.0f32;
    for i in 0..n {
        let Some(sw) = skin.weights.get(sv.weight_begin as usize + i) else {
            break;
        };
        joints[i] = sw.cluster_index.min(u32::from(u16::MAX)) as u16;
        weights[i] = sw.weight as f32;
        sum += weights[i];
    }
    if sum > 1e-8 {
        for w in &mut weights {
            *w /= sum;
        }
    }
    (joints, weights)
}

fn push_skin(
    scene: &mut Scene,
    skin: &ufbx::SkinDeformer,
    id_to_index: &HashMap<u32, usize>,
) -> usize {
    let mut joints = Vec::new();
    let mut inverse_bind = Vec::new();
    for cluster in &skin.clusters {
        let node_index = cluster
            .bone_node
            .as_ref()
            .and_then(|n| id_to_index.get(&n.element.element_id).copied())
            .unwrap_or(0);
        if let Some(node) = scene.nodes.get_mut(node_index) {
            node.is_bone = true;
        }
        joints.push(node_index);
        inverse_bind.push(mat_from(cluster.geometry_to_bone).to_cols_array());
    }
    let idx = scene.skins.len();
    scene.skins.push(Skin {
        name: element_name(&skin.element.name, "skin"),
        joints,
        inverse_bind,
    });
    idx
}

fn convert_material(mat: &ufbx::Material, tex_ids: &HashMap<u32, usize>) -> Material {
    let base = if mat.pbr.base_color.has_value {
        map_color(&mat.pbr.base_color)
    } else {
        map_color(&mat.fbx.diffuse_color)
    };
    let factor = if mat.pbr.base_factor.has_value {
        mat.pbr.base_factor.value_vec4.x as f32
    } else if mat.fbx.diffuse_factor.has_value {
        mat.fbx.diffuse_factor.value_vec4.x as f32
    } else {
        1.0
    };
    let opacity = if mat.pbr.opacity.has_value {
        mat.pbr.opacity.value_vec4.x as f32
    } else {
        1.0
    };
    let roughness = if mat.features.roughness_as_glossiness.enabled && mat.pbr.glossiness.has_value
    {
        1.0 - (mat.pbr.glossiness.value_vec4.x as f32).clamp(0.0, 1.0)
    } else if mat.pbr.roughness.has_value {
        (mat.pbr.roughness.value_vec4.x as f32).clamp(0.0, 1.0)
    } else {
        0.55
    };
    let metallic = if mat.pbr.metalness.has_value {
        (mat.pbr.metalness.value_vec4.x as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let tex = mat
        .pbr
        .base_color
        .texture
        .as_ref()
        .filter(|_| mat.pbr.base_color.texture_enabled)
        .and_then(|t| tex_ids.get(&t.element.element_id).copied())
        .or_else(|| {
            mat.fbx
                .diffuse_color
                .texture
                .as_ref()
                .filter(|_| mat.fbx.diffuse_color.texture_enabled)
                .and_then(|t| tex_ids.get(&t.element.element_id).copied())
        });
    Material {
        name: element_name(&mat.element.name, "material"),
        base_color: [
            (base[0] * factor).clamp(0.0, 1.0),
            (base[1] * factor).clamp(0.0, 1.0),
            (base[2] * factor).clamp(0.0, 1.0),
            opacity.clamp(0.0, 1.0),
        ],
        metallic,
        roughness,
        base_color_texture: tex,
        shading_model: {
            let name = ustr(&mat.shading_model_name);
            if name.is_empty() {
                format!("{:?}", mat.shader_type)
            } else {
                name
            }
        },
        double_sided: mat.features.double_sided.enabled,
        alpha_mode: "opaque".into(),
        alpha_cutoff: 0.5,
    }
}

fn push_texture(scene: &mut Scene, model: &Path, tex: &ufbx::Texture) -> usize {
    let referenced = {
        let rel = ustr(&tex.relative_filename);
        let file = ustr(&tex.filename);
        let abs = ustr(&tex.absolute_filename);
        if !rel.is_empty() {
            rel
        } else if !file.is_empty() {
            file
        } else {
            abs
        }
    };
    let mut embedded_bytes = blob_vec(&tex.content);
    if embedded_bytes.is_empty() {
        if let Some(video) = tex.video.as_ref() {
            embedded_bytes = blob_vec(&video.content);
        }
    }
    let name = element_name(&tex.element.name, "texture");
    let mut texture = Texture {
        name,
        referenced_path: referenced.clone(),
        resolved_path: None,
        embedded: !embedded_bytes.is_empty(),
        status: "missing".into(),
        searched: Vec::new(),
        width: 0,
        height: 0,
        rgba: Vec::new(),
    };
    if !embedded_bytes.is_empty() {
        match decode_bytes_named(&embedded_bytes, Some(&referenced)) {
            Some((w, h, rgba)) => {
                texture.status = "ok".into();
                texture.width = w;
                texture.height = h;
                texture.rgba = rgba;
            }
            None => {
                texture.status = "decode_failed".into();
                scene.add_problem(Problem::warning(
                    "TEXTURE_DECODE_FAILED",
                    format!("embedded texture '{}' could not be decoded", texture.name),
                ));
            }
        }
    } else if !referenced.is_empty() {
        let found = textures::resolve(model, &referenced);
        texture.searched = found.searched.clone();
        if found.absolute_reference {
            scene.add_problem(Problem::warning(
                "TEXTURE_ABSOLUTE_PATH",
                textures::absolute_path_message(&referenced, &found),
            ));
        }
        if let Some(path) = found.resolved {
            texture.resolved_path = Some(path.display().to_string());
            match fs::read(&path) {
                Ok(bytes) => match decode_bytes(&bytes) {
                    Some((w, h, rgba)) => {
                        texture.status = "ok".into();
                        texture.width = w;
                        texture.height = h;
                        texture.rgba = rgba;
                    }
                    None => {
                        texture.status = "decode_failed".into();
                        scene.add_problem(Problem::warning(
                            "TEXTURE_DECODE_FAILED",
                            format!("could not decode {}", path.display()),
                        ));
                    }
                },
                Err(err) => {
                    scene.add_problem(Problem::warning(
                        "TEXTURE_MISSING",
                        format!("texture '{referenced}' could not be read ({err})"),
                    ));
                }
            }
        } else {
            let mut problem = Problem::warning(
                "TEXTURE_MISSING",
                format!("texture '{referenced}' was not found"),
            );
            problem.path = Some(referenced);
            problem.searched = found.searched;
            scene.add_problem(problem);
        }
    }
    let idx = scene.textures.len();
    scene.textures.push(texture);
    idx
}

fn map_color(map: &ufbx::MaterialMap) -> [f32; 4] {
    let v = map.value_vec4;
    [
        v.x as f32,
        v.y as f32,
        v.z as f32,
        if v.w == 0.0 { 1.0 } else { v.w as f32 },
    ]
}

fn v3(stream: &ufbx::VertexVec3, index: usize) -> [f32; 3] {
    if !stream.exists || index >= stream.indices.len() {
        return [0.0, 0.0, 0.0];
    }
    let v = &stream[index];
    [v.x as f32, v.y as f32, v.z as f32]
}

fn v2(stream: &ufbx::VertexVec2, index: usize) -> [f32; 2] {
    if !stream.exists || index >= stream.indices.len() {
        return [0.0, 0.0];
    }
    let v = &stream[index];
    [v.x as f32, v.y as f32]
}

fn mat_from(m: ufbx::Matrix) -> Mat4 {
    Mat4::from_cols(
        Vec4::new(m.m00 as f32, m.m10 as f32, m.m20 as f32, 0.0),
        Vec4::new(m.m01 as f32, m.m11 as f32, m.m21 as f32, 0.0),
        Vec4::new(m.m02 as f32, m.m12 as f32, m.m22 as f32, 0.0),
        Vec4::new(m.m03 as f32, m.m13 as f32, m.m23 as f32, 1.0),
    )
}

fn trs_from_matrix(m: Mat4) -> Trs {
    let (scale, rotation, translation) = m.to_scale_rotation_translation();
    Trs {
        translation: translation.to_array(),
        rotation: [rotation.x, rotation.y, rotation.z, rotation.w],
        scale: scale.to_array(),
    }
}

fn trs_from_ufbx(t: ufbx::Transform) -> Trs {
    Trs {
        translation: [
            t.translation.x as f32,
            t.translation.y as f32,
            t.translation.z as f32,
        ],
        rotation: [
            t.rotation.x as f32,
            t.rotation.y as f32,
            t.rotation.z as f32,
            t.rotation.w as f32,
        ],
        scale: [t.scale.x as f32, t.scale.y as f32, t.scale.z as f32],
    }
}

fn trs_differs(a: Trs, b: Trs) -> bool {
    let far = |x: f32, y: f32| (x - y).abs() > 1e-4;
    if a.translation
        .iter()
        .zip(b.translation)
        .any(|(x, y)| far(*x, y))
    {
        return true;
    }
    if a.scale.iter().zip(b.scale).any(|(x, y)| far(*x, y)) {
        return true;
    }
    let dot = a
        .rotation
        .iter()
        .zip(b.rotation)
        .map(|(x, y)| x * y)
        .sum::<f32>()
        .abs();
    dot < 0.9999
}

fn ustr(s: &ufbx::String) -> String {
    s.as_ref().to_owned()
}

fn element_name(s: &ufbx::String, fallback: &str) -> String {
    let name = ustr(s);
    if name.is_empty() {
        fallback.to_string()
    } else {
        name
    }
}

fn blob_vec(blob: &ufbx::Blob) -> Vec<u8> {
    if blob.size == 0 {
        Vec::new()
    } else {
        blob.to_vec()
    }
}

fn axis_name(axis: ufbx::CoordinateAxis) -> &'static str {
    match axis {
        ufbx::CoordinateAxis::PositiveX => "+X",
        ufbx::CoordinateAxis::NegativeX => "-X",
        ufbx::CoordinateAxis::PositiveY => "+Y",
        ufbx::CoordinateAxis::NegativeY => "-Y",
        ufbx::CoordinateAxis::PositiveZ => "+Z",
        ufbx::CoordinateAxis::NegativeZ => "-Z",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/ufbx")
            .join(name)
    }

    #[test]
    fn ascii_6100_default_and_binary_7400() {
        let ascii = load(&corpus("blender_279_default_6100_ascii.fbx")).unwrap();
        assert_eq!(ascii.source.fbx_version, Some(6100));
        assert_eq!(ascii.source.fbx_ascii, Some(true));
        assert!(ascii.triangle_count() > 0);
        assert!(ascii.problems.iter().any(|p| p.code == "ASCII_FBX"));
        assert!(ascii.problems.iter().any(|p| p.code == "OLD_FBX_VERSION"));
        let binary = load(&corpus("blender_279_default_7400_binary.fbx")).unwrap();
        assert_eq!(binary.source.fbx_version, Some(7400));
        assert_eq!(binary.source.fbx_ascii, Some(false));
        assert!(binary.triangle_count() > 0);
    }

    #[test]
    fn sausage_is_skinned_and_animated() {
        for name in [
            "blender_279_sausage_6100_ascii.fbx",
            "blender_279_sausage_7400_binary.fbx",
        ] {
            let mut scene = load(&corpus(name)).unwrap();
            scene.recompute_bounds();
            assert!(
                scene.bone_count() >= 3,
                "{name} bones {}",
                scene.bone_count()
            );
            assert!(!scene.skins.is_empty(), "{name} has no skin");
            assert!(
                scene.animations.len() >= 2,
                "{name} anims {:?}",
                scene.animations.iter().map(|a| &a.name).collect::<Vec<_>>()
            );
            assert!(
                scene.bounds.radius() > 1e-4 && scene.bounds.radius() < 1_000.0,
                "{name} radius {}",
                scene.bounds.radius()
            );
        }
    }

    fn cm_skinned_fbx(geo_scale: f64) -> String {
        // 180cm column, hips at the origin, head 170cm up. UnitScaleFactor 1
        // means one centimetre per file unit. The cluster matrix is a rotation
        // plus a centimetre translation, matching Maya and Unreal bind poses.
        format!(
            r#"; FBX 7.7.0 project file
FBXHeaderExtension:  {{
	FBXHeaderVersion: 1004
	FBXVersion: 7700
	Creator: "modelview synthetic"
}}
GlobalSettings:  {{
	Version: 1000
	Properties70:  {{
		P: "UpAxis", "int", "Integer", "",1
		P: "UpAxisSign", "int", "Integer", "",1
		P: "FrontAxis", "int", "Integer", "",2
		P: "FrontAxisSign", "int", "Integer", "",1
		P: "CoordAxis", "int", "Integer", "",0
		P: "CoordAxisSign", "int", "Integer", "",1
		P: "UnitScaleFactor", "double", "Number", "",1
		P: "OriginalUnitScaleFactor", "double", "Number", "",1
	}}
}}
Objects:  {{
	Geometry: 10, "Geometry::Body", "Mesh" {{
		Vertices: *12 {{
			a: -10,0,0, 10,0,0, 10,180,0, -10,180,0
		}}
		PolygonVertexIndex: *6 {{
			a: 0,1,2,-4
		}}
		GeometryVersion: 124
	}}
	Model: 20, "Model::Hips", "LimbNode" {{
		Version: 232
		Properties70:  {{
			P: "Lcl Translation", "Lcl Translation", "", "A",0,0,0
			P: "Lcl Rotation", "Lcl Rotation", "", "A",0,0,0
			P: "Lcl Scaling", "Lcl Scaling", "", "A",1,1,1
		}}
	}}
	Model: 21, "Model::Head", "LimbNode" {{
		Version: 232
		Properties70:  {{
			P: "Lcl Translation", "Lcl Translation", "", "A",0,170,0
			P: "Lcl Rotation", "Lcl Rotation", "", "A",0,0,0
			P: "Lcl Scaling", "Lcl Scaling", "", "A",1,1,1
		}}
	}}
	Model: 22, "Model::Body", "Mesh" {{
		Version: 232
		Properties70:  {{
			P: "Lcl Translation", "Lcl Translation", "", "A",0,0,0
			P: "Lcl Rotation", "Lcl Rotation", "", "A",0,0,0
			P: "Lcl Scaling", "Lcl Scaling", "", "A",1,1,1
			P: "GeometricScaling", "Vector3D", "Vector", "",{geo_scale},{geo_scale},{geo_scale}
		}}
	}}
	Deformer: 30, "Deformer::Skin", "Skin" {{
		Version: 101
		Link_DeformAcuracy: 50
		SkinningType: "Linear"
	}}
	Deformer: 31, "SubDeformer::HipsCluster", "Cluster" {{
		Version: 100
		Indexes: *4 {{
			a: 0,1,2,3
		}}
		Weights: *4 {{
			a: 1,1,1,1
		}}
		Transform: *16 {{
			a: 0,1,0,0, -1,0,0,0, 0,0,1,0, 0,-90,0,1
		}}
		TransformLink: *16 {{
			a: 1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1
		}}
	}}
}}
Connections:  {{
	C: "OO",20,0
	C: "OO",21,20
	C: "OO",22,0
	C: "OO",10,22
	C: "OO",30,10
	C: "OO",31,30
	C: "OO",20,31
}}
"#
        )
    }

    fn load_cm() -> Scene {
        let dir = std::env::temp_dir().join(format!("mv-skin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("char.fbx");
        // Bind is a 90 degree rotation plus a centimetre translation, which is
        // how Maya and Unreal write cluster matrices. The 3x3 scale is 1.
        fs::write(&path, cm_skinned_fbx(1.0)).unwrap();
        let mut scene = load(&path).unwrap();
        scene.recompute_bounds();
        crate::validate::add_geometry_problems(&mut scene);
        let _ = fs::remove_dir_all(&dir);
        scene
    }

    #[test]
    fn centimetre_cluster_translation_stays_with_the_skeleton() {
        let scene = load_cm();
        let head = scene
            .world_matrices(None, 0.0)
            .get(scene.nodes.iter().position(|n| n.name == "Head").unwrap())
            .copied()
            .unwrap();
        assert!(
            (head.w_axis.y - 1.7).abs() < 0.08,
            "skeleton head {}",
            head.w_axis.y
        );
        let extent = scene.bounds.max_extent();
        assert!(
            (1.2..2.6).contains(&extent),
            "mesh extent {extent} bounds {:?}",
            scene.bounds
        );
        assert!(
            scene
                .problems
                .iter()
                .all(|p| p.code != "MESH_SKELETON_MISMATCH"),
            "{:?}",
            scene.problems
        );
    }

    fn ufbx_skin_extent(path: &std::path::Path) -> f32 {
        let path_str = path.to_str().unwrap();
        let mut opts = ufbx::LoadOpts::default();
        opts.target_unit_meters = 1.0;
        opts.target_axes = ufbx::CoordinateAxes::right_handed_y_up();
        opts.space_conversion = ufbx::SpaceConversion::ModifyGeometry;
        opts.geometry_transform_handling = ufbx::GeometryTransformHandling::ModifyGeometry;
        let loaded = ufbx::load_file(path_str, opts).unwrap();
        let mesh = &loaded.meshes[0];
        let node = &mesh.element.instances[0];
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for corner in 0..mesh.num_indices {
            let local = mesh.skinned_position[corner];
            let pos = ufbx::transform_position(&node.geometry_to_world, local);
            let p = [pos.x as f32, pos.y as f32, pos.z as f32];
            for c in 0..3 {
                min[c] = min[c].min(p[c]);
                max[c] = max[c].max(p[c]);
            }
        }
        (0..3).map(|c| max[c] - min[c]).fold(0.0, f32::max)
    }

    #[test]
    fn tiny_file_matches_ufbx_and_is_not_rescaled() {
        let dir = std::env::temp_dir().join(format!("mv-tiny-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 2cm column. Same cluster shape as the metre fixture, translations in centimetres.
        let text = cm_skinned_fbx(1.0)
            .replace(
                "-10,0,0, 10,0,0, 10,180,0, -10,180,0",
                "-1,0,0, 1,0,0, 1,2,0, -1,2,0",
            )
            .replace("0,170,0", "0,1.7,0")
            .replace("0,-90,0,1", "0,-1,0,1");
        let path = dir.join("tiny.fbx");
        fs::write(&path, text).unwrap();
        let mut scene = load(&path).unwrap();
        scene.recompute_bounds();
        crate::validate::add_geometry_problems(&mut scene);
        crate::validate::flag_unusual_scale(&mut scene);
        let extent = scene.bounds.max_extent();
        let ufbx_extent = ufbx_skin_extent(&path);
        assert!(
            (extent - ufbx_extent).abs() < 0.005,
            "our extent {extent} ufbx {ufbx_extent}"
        );
        assert!(
            (0.01..0.05).contains(&extent),
            "tiny file was rescaled to {extent}"
        );
        assert!(
            scene.problems.iter().any(|p| p.code == "UNUSUAL_SCALE"),
            "{:?}",
            scene.problems
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_7700_ascii_and_uv_sets() {
        let file = load(&corpus("synthetic_direct_by_polygon_7700_ascii.fbx")).unwrap();
        assert_eq!(file.source.fbx_version, Some(7700));
        assert_eq!(file.source.fbx_ascii, Some(true));
        assert!(file.triangle_count() > 0);
        let uvs = load(&corpus("blender_279_uv_sets_7400_binary.fbx")).unwrap();
        assert!(
            uvs.meshes.iter().any(|m| m.uvs.len() >= 2),
            "expected multiple UV sets"
        );
    }
}
