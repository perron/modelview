use std::fs;
use std::path::Path;

use glam::Mat4;

use crate::error::Result;
use crate::load::textures::{self, decode_bytes};
use crate::scene::{
    push_checked, Animation, Material, Mesh, Node, Problem, Scene, Skin, Texture, Track, Trs, UvSet,
};
use crate::MvError;

pub fn load(path: &Path) -> Result<Scene> {
    let bytes = fs::read(path).map_err(MvError::io)?;
    let gltf = gltf::Gltf::from_slice(&bytes).map_err(|err| MvError::parse(err.to_string()))?;
    let buffers = gltf::import_buffers(&gltf.document, path.parent(), gltf.blob)
        .map_err(|err| MvError::parse(format!("glTF buffers: {err}")))?;
    let document = &gltf.document;

    let mut scene = Scene::new(
        if path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .eq_ignore_ascii_case("glb")
        {
            "glb"
        } else {
            "gltf"
        },
    );

    let image_to_tex = load_images(&mut scene, path, document, &buffers)?;
    let mut materials = Vec::new();
    for mat in document.materials() {
        materials.push(convert_material(&mat, &image_to_tex));
    }
    scene.materials = materials;

    let mesh_map = load_meshes(&mut scene, document, &buffers)?;
    load_skins(&mut scene, document, &buffers);
    load_nodes(&mut scene, document, &mesh_map);
    load_animations(&mut scene, document, &buffers);
    if document.scenes().len() > 1 {
        scene.add_problem(Problem::info(
            "UNSUPPORTED_FEATURE",
            format!(
                "file has {} scenes; the default scene is shown",
                document.scenes().len()
            ),
        ));
    }
    Ok(scene)
}

fn buf<'a>(buffers: &'a [gltf::buffer::Data], buffer: gltf::Buffer<'_>) -> &'a [u8] {
    buffers
        .get(buffer.index())
        .map(|d| d.0.as_slice())
        .unwrap_or(b"")
}

fn load_images(
    scene: &mut Scene,
    model: &Path,
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
) -> Result<Vec<Option<usize>>> {
    let mut map = Vec::new();
    for image in document.images() {
        let name = image.name().unwrap_or("image").to_string();
        let (embedded, referenced, bytes) = match image.source() {
            gltf::image::Source::View { view, .. } => {
                let data = buf(buffers, view.buffer());
                let start = view.offset();
                let end = start.saturating_add(view.length()).min(data.len());
                (
                    true,
                    name.clone(),
                    data.get(start..end).unwrap_or(b"").to_vec(),
                )
            }
            gltf::image::Source::Uri { uri, .. } => {
                if let Some(rest) = uri.strip_prefix("data:") {
                    let encoded = rest.split_once(',').map(|(_, data)| data).unwrap_or(rest);
                    let decoded = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        encoded.trim(),
                    )
                    .unwrap_or_default();
                    (true, uri.to_string(), decoded)
                } else {
                    (false, uri.to_string(), Vec::new())
                }
            }
        };
        let mut texture = Texture {
            name,
            referenced_path: referenced.clone(),
            resolved_path: None,
            embedded,
            status: "missing".into(),
            searched: Vec::new(),
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        let decoded = if !bytes.is_empty() {
            decode_bytes(&bytes)
        } else if !embedded {
            let found = textures::resolve(model, &referenced);
            texture.searched = found.searched.clone();
            if found.absolute_reference {
                scene.add_problem(Problem::warning(
                    "TEXTURE_ABSOLUTE_PATH",
                    format!("texture '{referenced}' uses an absolute path"),
                ));
            }
            if let Some(path) = found.resolved {
                texture.resolved_path = Some(path.display().to_string());
                fs::read(&path).ok().and_then(|b| decode_bytes(&b))
            } else {
                let mut problem = Problem::warning(
                    "TEXTURE_MISSING",
                    format!("texture '{referenced}' was not found"),
                );
                problem.path = Some(referenced.clone());
                problem.searched = found.searched;
                scene.add_problem(problem);
                None
            }
        } else {
            None
        };
        match decoded {
            Some((w, h, rgba)) => {
                texture.status = "ok".into();
                texture.width = w;
                texture.height = h;
                texture.rgba = rgba;
            }
            None if texture.status != "missing" || embedded => {
                if embedded {
                    texture.status = "decode_failed".into();
                    scene.add_problem(Problem::warning(
                        "TEXTURE_DECODE_FAILED",
                        format!("could not decode image '{}'", texture.name),
                    ));
                }
            }
            None => {}
        }
        map.push(Some(scene.textures.len()));
        scene.textures.push(texture);
    }
    Ok(map)
}

fn convert_material(mat: &gltf::Material, images: &[Option<usize>]) -> Material {
    let pbr = mat.pbr_metallic_roughness();
    let factor = pbr.base_color_factor();
    let tex = pbr.base_color_texture().and_then(|info| {
        images
            .get(info.texture().source().index())
            .copied()
            .flatten()
    });
    Material {
        name: mat.name().unwrap_or("material").to_string(),
        base_color: factor,
        metallic: pbr.metallic_factor(),
        roughness: pbr.roughness_factor(),
        base_color_texture: tex,
        shading_model: "pbr-metallic-roughness".into(),
        double_sided: mat.double_sided(),
    }
}

fn load_meshes(
    scene: &mut Scene,
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
) -> Result<Vec<Vec<usize>>> {
    let mut map = Vec::new();
    for mesh in document.meshes() {
        let mut ours = Vec::new();
        for (pi, primitive) in mesh.primitives().enumerate() {
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                scene.add_problem(Problem::info(
                    "UNSUPPORTED_FEATURE",
                    format!(
                        "mesh '{}' primitive {pi} uses {:?} and was skipped",
                        mesh.name().unwrap_or("mesh"),
                        primitive.mode()
                    ),
                ));
                continue;
            }
            let reader = primitive.reader(|buffer| Some(buf(buffers, buffer)));
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            let positions: Vec<[f32; 3]> = positions.collect();
            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|i| i.collect())
                .unwrap_or_default();
            let mut uvs = Vec::new();
            for set in 0..4 {
                let reader = primitive.reader(|buffer| Some(buf(buffers, buffer)));
                if let Some(coords) = reader.read_tex_coords(set) {
                    uvs.push(UvSet {
                        name: format!("TEXCOORD_{set}"),
                        coords: coords.into_f32().collect(),
                    });
                } else {
                    break;
                }
            }
            let reader = primitive.reader(|buffer| Some(buf(buffers, buffer)));
            let indices: Vec<u32> = if let Some(indices) = reader.read_indices() {
                indices.into_u32().collect()
            } else {
                (0..positions.len() as u32).collect()
            };
            let mut built = Mesh::new(mesh.name().unwrap_or("mesh"));
            built.positions = positions;
            built.normals = if normals.len() == built.positions.len() {
                normals
            } else {
                vec![[0.0, 1.0, 0.0]; built.positions.len()]
            };
            built.uvs = uvs;
            built.indices = indices;
            built.material = primitive
                .material()
                .index()
                .and_then(|i| (i < scene.materials.len()).then_some(i));
            read_skin_attrs(&mut built, &primitive, buffers);
            if built.normals.iter().all(|n| *n == [0.0, 1.0, 0.0]) {
                // filled below if we generated placeholders
            }
            let mut tris = 0usize;
            for _ in built.indices.chunks_exact(3) {
                push_checked(&mut tris)?;
            }
            if built.normals.iter().all(|n| *n == [0.0, 1.0, 0.0]) {
                generate_normals(&mut built);
            }
            ours.push(scene.meshes.len());
            scene.meshes.push(built);
        }
        map.push(ours);
    }
    Ok(map)
}

fn read_skin_attrs(mesh: &mut Mesh, primitive: &gltf::Primitive, buffers: &[gltf::buffer::Data]) {
    let reader = primitive.reader(|buffer| Some(buf(buffers, buffer)));
    let Some(joints) = reader.read_joints(0) else {
        return;
    };
    let joints: Vec<[u16; 4]> = match joints {
        gltf::mesh::util::ReadJoints::U8(it) => it
            .map(|j| [j[0] as u16, j[1] as u16, j[2] as u16, j[3] as u16])
            .collect(),
        gltf::mesh::util::ReadJoints::U16(it) => it.collect(),
    };
    let reader = primitive.reader(|buffer| Some(buf(buffers, buffer)));
    let weights: Vec<[f32; 4]> = reader
        .read_weights(0)
        .map(|w| w.into_f32().collect())
        .unwrap_or_else(|| vec![[1.0, 0.0, 0.0, 0.0]; joints.len()]);
    if joints.len() == mesh.positions.len() {
        mesh.joints = joints;
        mesh.weights = weights;
    }
}

fn generate_normals(mesh: &mut Mesh) {
    let mut acc = vec![glam::Vec3::ZERO; mesh.positions.len()];
    for tri in mesh.indices.chunks_exact(3) {
        let i0 = tri[0] as usize;
        let i1 = tri[1] as usize;
        let i2 = tri[2] as usize;
        if i0 >= mesh.positions.len() || i1 >= mesh.positions.len() || i2 >= mesh.positions.len() {
            continue;
        }
        let a = glam::Vec3::from(mesh.positions[i0]);
        let b = glam::Vec3::from(mesh.positions[i1]);
        let c = glam::Vec3::from(mesh.positions[i2]);
        let n = (b - a).cross(c - a);
        acc[i0] += n;
        acc[i1] += n;
        acc[i2] += n;
    }
    for (i, n) in acc.into_iter().enumerate() {
        let n = n.normalize_or_zero();
        mesh.normals[i] = if n.length_squared() > 0.0 {
            n.to_array()
        } else {
            [0.0, 1.0, 0.0]
        };
    }
}

fn load_skins(scene: &mut Scene, document: &gltf::Document, buffers: &[gltf::buffer::Data]) {
    for skin in document.skins() {
        let joints: Vec<usize> = skin.joints().map(|n| n.index()).collect();
        let reader = skin.reader(|buffer| Some(buf(buffers, buffer)));
        let mut inverse_bind: Vec<[f32; 16]> = reader
            .read_inverse_bind_matrices()
            .map(|iter| {
                iter.map(|m| Mat4::from_cols_array_2d(&m).to_cols_array())
                    .collect()
            })
            .unwrap_or_default();
        if inverse_bind.len() != joints.len() {
            inverse_bind = vec![Mat4::IDENTITY.to_cols_array(); joints.len()];
        }
        for &node in &joints {
            if let Some(node) = scene.nodes.get_mut(node) {
                // nodes are not created yet; mark later
                let _ = node;
            }
        }
        scene.skins.push(Skin {
            name: skin.name().unwrap_or("skin").to_string(),
            joints,
            inverse_bind,
        });
    }
}

fn load_nodes(scene: &mut Scene, document: &gltf::Document, mesh_map: &[Vec<usize>]) {
    let count = document.nodes().len();
    let mut parents = vec![None; count];
    for node in document.nodes() {
        for child in node.children() {
            if child.index() < count {
                parents[child.index()] = Some(node.index());
            }
        }
    }
    for node in document.nodes() {
        let rest = match node.transform() {
            gltf::scene::Transform::Matrix { matrix } => {
                trs_from_matrix(Mat4::from_cols_array_2d(&matrix))
            }
            gltf::scene::Transform::Decomposed {
                translation,
                rotation,
                scale,
            } => Trs {
                translation,
                rotation,
                scale,
            },
        };
        let meshes = node
            .mesh()
            .and_then(|m| mesh_map.get(m.index()))
            .cloned()
            .unwrap_or_default();
        let skin = node
            .skin()
            .map(|s| s.index())
            .filter(|i| *i < scene.skins.len());
        scene.nodes.push(Node {
            name: node.name().unwrap_or("node").to_string(),
            parent: parents.get(node.index()).copied().flatten(),
            children: node.children().map(|c| c.index()).collect(),
            rest,
            meshes: meshes.clone(),
            is_bone: false,
        });
        if let Some(skin_index) = skin {
            for &mi in &meshes {
                if let Some(mesh) = scene.meshes.get_mut(mi) {
                    if mesh.skin.is_none() {
                        mesh.skin = Some(skin_index);
                    }
                }
            }
            if let Some(skin) = scene.skins.get(skin_index) {
                let joint_nodes = skin.joints.clone();
                // nodes for joints may not all be pushed yet if joint index > current.
                // Mark after the loop.
                let _ = joint_nodes;
            }
        }
    }
    let joint_nodes: Vec<usize> = scene
        .skins
        .iter()
        .flat_map(|s| s.joints.iter().copied())
        .collect();
    for node in joint_nodes {
        if let Some(node) = scene.nodes.get_mut(node) {
            node.is_bone = true;
        }
    }
    // Also mark ancestors of bones so the chain draws.
    let mut changed = true;
    while changed {
        changed = false;
        let bone_parents: Vec<Option<usize>> = scene
            .nodes
            .iter()
            .map(|n| if n.is_bone { n.parent } else { None })
            .collect();
        for parent in bone_parents.into_iter().flatten() {
            if let Some(node) = scene.nodes.get_mut(parent) {
                if !node.is_bone && node.meshes.is_empty() {
                    node.is_bone = true;
                    changed = true;
                }
            }
        }
    }
}

fn trs_from_matrix(m: Mat4) -> Trs {
    let (scale, rotation, translation) = m.to_scale_rotation_translation();
    Trs {
        translation: translation.to_array(),
        rotation: [rotation.x, rotation.y, rotation.z, rotation.w],
        scale: scale.to_array(),
    }
}

fn load_animations(scene: &mut Scene, document: &gltf::Document, buffers: &[gltf::buffer::Data]) {
    for anim in document.animations() {
        let mut cubic = false;
        // node -> (times, translation keys, rotation keys, scale keys) sparse
        let mut per_node: Vec<ChannelAcc> = (0..scene.nodes.len())
            .map(|_| ChannelAcc::default())
            .collect();
        for channel in anim.channels() {
            let node = channel.target().node().index();
            if node >= per_node.len() {
                continue;
            }
            let sampler = channel.sampler();
            if matches!(
                sampler.interpolation(),
                gltf::animation::Interpolation::CubicSpline
            ) {
                cubic = true;
            }
            let reader = channel.reader(|buffer| Some(buf(buffers, buffer)));
            let mut times: Vec<f32> = reader
                .read_inputs()
                .map(|inputs| inputs.collect())
                .unwrap_or_default();
            let outputs = reader.read_outputs();
            let cubic_stride = matches!(
                sampler.interpolation(),
                gltf::animation::Interpolation::CubicSpline
            );
            if cubic_stride {
                // glTF cubic spline outputs are in-tangent, value, out-tangent.
                times = times.clone();
            }
            match (channel.target().property(), outputs) {
                (
                    gltf::animation::Property::Translation,
                    Some(gltf::animation::util::ReadOutputs::Translations(values)),
                ) => {
                    let values: Vec<[f32; 3]> = values.collect();
                    let values = take_cubic(&values, cubic_stride);
                    per_node[node].translation = Some((times, values));
                }
                (
                    gltf::animation::Property::Rotation,
                    Some(gltf::animation::util::ReadOutputs::Rotations(values)),
                ) => {
                    let values: Vec<[f32; 4]> = values.into_f32().collect();
                    let values = take_cubic(&values, cubic_stride);
                    per_node[node].rotation = Some((times, values));
                }
                (
                    gltf::animation::Property::Scale,
                    Some(gltf::animation::util::ReadOutputs::Scales(values)),
                ) => {
                    let values: Vec<[f32; 3]> = values.collect();
                    let values = take_cubic(&values, cubic_stride);
                    per_node[node].scale = Some((times, values));
                }
                (gltf::animation::Property::MorphTargetWeights, _) => {
                    scene.add_problem(Problem::info(
                        "BLEND_SHAPES_NOT_APPLIED",
                        format!("animation '{}' has morph target weights, which are not applied in v0.1", anim.name().unwrap_or("animation")),
                    ));
                }
                _ => {}
            }
        }
        if cubic {
            scene.add_problem(Problem::info(
                "ANIM_CUBIC_SPLINE_LINEARIZED",
                format!("animation '{}' uses cubic spline interpolation; v0.1 samples the values linearly", anim.name().unwrap_or("animation")),
            ));
        }
        let duration = per_node
            .iter()
            .flat_map(|c| {
                c.translation
                    .iter()
                    .flat_map(|(t, _)| t.last().copied())
                    .chain(c.rotation.iter().flat_map(|(t, _)| t.last().copied()))
                    .chain(c.scale.iter().flat_map(|(t, _)| t.last().copied()))
            })
            .fold(0.0f32, f32::max);
        let fps = 30.0f32;
        let samples = if duration < 1e-5 {
            1
        } else {
            ((duration * fps).round() as usize).clamp(1, 720)
        };
        let mut tracks = Vec::new();
        for (node, acc) in per_node.iter().enumerate() {
            if acc.translation.is_none() && acc.rotation.is_none() && acc.scale.is_none() {
                continue;
            }
            let rest = scene.nodes[node].rest;
            let mut times = Vec::new();
            let mut translation = Vec::new();
            let mut rotation = Vec::new();
            let mut scale = Vec::new();
            for s in 0..=samples {
                let time = if samples == 0 {
                    0.0
                } else {
                    duration * (s as f32) / (samples as f32)
                };
                times.push(time);
                translation.push(sample_vec3(
                    acc.translation.as_ref(),
                    time,
                    rest.translation,
                ));
                rotation.push(sample_quat(acc.rotation.as_ref(), time, rest.rotation));
                scale.push(sample_vec3(acc.scale.as_ref(), time, rest.scale));
            }
            tracks.push(Track {
                node,
                times,
                translation,
                rotation,
                scale,
            });
        }
        scene.animations.push(Animation {
            name: anim.name().unwrap_or("animation").to_string(),
            duration,
            fps,
            tracks,
        });
    }
}

#[derive(Default)]
struct ChannelAcc {
    translation: Option<(Vec<f32>, Vec<[f32; 3]>)>,
    rotation: Option<(Vec<f32>, Vec<[f32; 4]>)>,
    scale: Option<(Vec<f32>, Vec<[f32; 3]>)>,
}

fn take_cubic<T: Copy>(values: &[T], cubic: bool) -> Vec<T> {
    if !cubic {
        return values.to_vec();
    }
    values.iter().skip(1).step_by(3).copied().collect()
}

fn sample_vec3(keys: Option<&(Vec<f32>, Vec<[f32; 3]>)>, time: f32, rest: [f32; 3]) -> [f32; 3] {
    let Some((times, values)) = keys else {
        return rest;
    };
    if times.is_empty() || values.is_empty() {
        return rest;
    }
    sample_keyed(times, values, time, |a, b, u| {
        [
            a[0] + (b[0] - a[0]) * u,
            a[1] + (b[1] - a[1]) * u,
            a[2] + (b[2] - a[2]) * u,
        ]
    })
}

fn sample_quat(keys: Option<&(Vec<f32>, Vec<[f32; 4]>)>, time: f32, rest: [f32; 4]) -> [f32; 4] {
    let Some((times, values)) = keys else {
        return rest;
    };
    if times.is_empty() || values.is_empty() {
        return rest;
    }
    sample_keyed(times, values, time, |a, b, u| {
        let qa = glam::Quat::from_xyzw(a[0], a[1], a[2], a[3]);
        let qb = glam::Quat::from_xyzw(b[0], b[1], b[2], b[3]);
        let q = qa.slerp(qb, u);
        [q.x, q.y, q.z, q.w]
    })
}

fn sample_keyed<T: Copy>(
    times: &[f32],
    values: &[T],
    time: f32,
    mix: impl Fn(T, T, f32) -> T,
) -> T {
    let n = times.len().min(values.len());
    if n == 0 {
        return values[0];
    }
    if time <= times[0] || n == 1 {
        return values[0];
    }
    if time >= times[n - 1] {
        return values[n - 1];
    }
    let i = times[..n].partition_point(|t| *t < time).saturating_sub(1);
    let t0 = times[i];
    let t1 = times[(i + 1).min(n - 1)];
    let u = if t1 > t0 {
        (time - t0) / (t1 - t0)
    } else {
        0.0
    };
    mix(values[i], values[(i + 1).min(n - 1)], u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_cube_and_khronos_box() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cube = load(&root.join("testdata/generated/embedded_cube.glb")).unwrap();
        assert_eq!(cube.triangle_count(), 12);
        assert!(cube
            .textures
            .iter()
            .any(|t| t.embedded && t.status == "ok" && t.width > 0));
        let box_tex = load(&root.join("testdata/khronos/BoxTextured/BoxTextured.glb")).unwrap();
        assert!(box_tex.triangle_count() > 0);
        assert!(
            box_tex
                .textures
                .iter()
                .any(|t| t.embedded && t.status == "ok"),
            "khronos box texture missing"
        );
    }
}
