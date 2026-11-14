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
    let gltf = open_gltf(&bytes)?;
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

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "KHR_draco_mesh_compression",
    "KHR_texture_transform",
    "KHR_materials_unlit",
    "KHR_mesh_quantization",
];

fn open_gltf(bytes: &[u8]) -> Result<gltf::Gltf> {
    match gltf::Gltf::from_slice(bytes) {
        Ok(gltf) => Ok(gltf),
        Err(err) => {
            let loose = gltf::Gltf::from_slice_without_validation(bytes)
                .map_err(|_| MvError::parse(err.to_string()))?;
            if let Some(missing) = unsupported_extension(&loose.document) {
                return Err(missing);
            }
            // gltf-json rejects extensions it was not built with. Continue when
            // every required extension is one this loader implements.
            let required: Vec<&str> = loose.document.extensions_required().collect();
            if !required.is_empty()
                && required
                    .iter()
                    .all(|name| SUPPORTED_EXTENSIONS.contains(name))
            {
                Ok(loose)
            } else {
                Err(MvError::parse(err.to_string()))
            }
        }
    }
}

fn unsupported_extension(document: &gltf::Document) -> Option<MvError> {
    let missing: Vec<&str> = document
        .extensions_required()
        .filter(|name| !SUPPORTED_EXTENSIONS.contains(name))
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(MvError::new(
            "UNSUPPORTED_EXTENSION",
            format!(
                "required glTF extension(s) not supported: {}",
                missing.join(", ")
            ),
        ))
    }
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
                    textures::absolute_path_message(&referenced, &found),
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

fn attribute_id(attributes: &serde_json::Value, name: &str) -> Option<u32> {
    attributes
        .get(name)
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
}

fn draco_vec3(mesh: &draco_core::Mesh, id: Option<u32>) -> Result<Vec<[f32; 3]>> {
    let Some(id) = id else {
        return Err(MvError::parse("Draco mesh has no POSITION attribute"));
    };
    let attr = mesh
        .attribute_by_unique_id(id)
        .ok_or_else(|| MvError::parse(format!("Draco attribute {id} is missing")))?;
    let points = mesh.num_points();
    let raw = attr.read_f32s(points, 3);
    Ok(raw.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect())
}

fn draco_vec2(mesh: &draco_core::Mesh, id: u32) -> Result<Vec<[f32; 2]>> {
    let attr = mesh
        .attribute_by_unique_id(id)
        .ok_or_else(|| MvError::parse(format!("Draco attribute {id} is missing")))?;
    let raw = attr.read_f32s(mesh.num_points(), 2);
    Ok(raw.chunks_exact(2).map(|c| [c[0], c[1]]).collect())
}

fn decode_draco_primitive(
    document: &gltf::Document,
    primitive: &gltf::Primitive<'_>,
    buffers: &[gltf::buffer::Data],
) -> Result<Option<(Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<UvSet>, Vec<u32>)>> {
    let Some(ext) = primitive.extension_value("KHR_draco_mesh_compression") else {
        return Ok(None);
    };
    let view_index = ext
        .get("bufferView")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| MvError::parse("KHR_draco_mesh_compression is missing bufferView"))?
        as usize;
    let view = document
        .views()
        .nth(view_index)
        .ok_or_else(|| MvError::parse("Draco bufferView is out of range"))?;
    let data = buf(buffers, view.buffer());
    let start = view.offset();
    let end = start.saturating_add(view.length()).min(data.len());
    let bytes = data.get(start..end).unwrap_or(b"");
    let mut decoded = draco_core::Mesh::new();
    draco_core::MeshDecoder::new()
        .decode(&mut draco_core::DecoderBuffer::new(bytes), &mut decoded)
        .map_err(|err| MvError::parse(format!("Draco decode failed: {err}")))?;
    let attributes = ext
        .get("attributes")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let positions = draco_vec3(&decoded, attribute_id(&attributes, "POSITION"))?;
    let normals = draco_vec3(&decoded, attribute_id(&attributes, "NORMAL")).unwrap_or_default();
    let mut uvs = Vec::new();
    for set in 0..4 {
        let name = format!("TEXCOORD_{set}");
        let Some(id) = attribute_id(&attributes, &name) else {
            break;
        };
        if let Ok(coords) = draco_vec2(&decoded, id) {
            uvs.push(UvSet { name, coords });
        }
    }
    let mut indices = Vec::with_capacity(decoded.num_faces() * 3);
    for face in decoded.faces() {
        for corner in face {
            indices.push(corner.0);
        }
    }
    if indices.is_empty() {
        indices = (0..positions.len() as u32).collect();
    }
    Ok(Some((positions, normals, uvs, indices)))
}

fn apply_texture_transform(primitive: &gltf::Primitive<'_>, uvs: &mut [UvSet]) {
    let Some(info) = primitive
        .material()
        .pbr_metallic_roughness()
        .base_color_texture()
    else {
        return;
    };
    let Some(ext) = info.extension_value("KHR_texture_transform") else {
        return;
    };
    let set = ext.get("texCoord").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let Some(uvs) = uvs.get_mut(set) else {
        return;
    };
    let offset = json_vec2(ext.get("offset"), [0.0, 0.0]);
    let scale = json_vec2(ext.get("scale"), [1.0, 1.0]);
    let rotation = ext.get("rotation").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
    let (sin, cos) = rotation.sin_cos();
    for uv in &mut uvs.coords {
        let u = uv[0];
        let v = uv[1];
        *uv = [
            offset[0] + scale[0] * (cos * u + sin * v),
            offset[1] + scale[1] * (-sin * u + cos * v),
        ];
    }
}

fn json_vec2(value: Option<&serde_json::Value>, default: [f32; 2]) -> [f32; 2] {
    let Some(value) = value else {
        return default;
    };
    [
        value
            .get(0)
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
            .unwrap_or(default[0]),
        value
            .get(1)
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
            .unwrap_or(default[1]),
    ]
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
            let draco = decode_draco_primitive(document, &primitive, buffers)?;
            let (positions, normals, mut uvs, indices) = if let Some(decoded) = draco {
                decoded
            } else {
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
                (positions, normals, uvs, indices)
            };
            apply_texture_transform(&primitive, &mut uvs);
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

    fn write_glb(dir: &std::path::Path, name: &str, json: &str, bin: &[u8]) -> std::path::PathBuf {
        let json_bytes = json.as_bytes();
        let json_pad = (4 - json_bytes.len() % 4) % 4;
        let bin_pad = (4 - bin.len() % 4) % 4;
        let json_len = json_bytes.len() + json_pad;
        let bin_len = bin.len() + bin_pad;
        let total = 12 + 8 + json_len + if bin.is_empty() { 0 } else { 8 + bin_len };
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json_len as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(json_bytes);
        out.extend(std::iter::repeat(b' ').take(json_pad));
        if !bin.is_empty() {
            out.extend_from_slice(&(bin_len as u32).to_le_bytes());
            out.extend_from_slice(b"BIN\0");
            out.extend_from_slice(bin);
            out.extend(std::iter::repeat(0u8).take(bin_pad));
        }
        let path = dir.join(name);
        fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn unsupported_extension_is_one_error() {
        let dir = std::env::temp_dir().join(format!("mv-ext-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let json = r#"{
            "asset": {"version": "2.0"},
            "extensionsUsed": ["MADE_UP_extension"],
            "extensionsRequired": ["MADE_UP_extension"],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "accessors": [{"componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}],
            "nodes": [{"mesh": 0}],
            "scenes": [{"nodes": [0]}],
            "scene": 0
        }"#;
        let path = write_glb(&dir, "bad.glb", json, &[]);
        let err = load(&path).unwrap_err();
        assert_eq!(err.code, "UNSUPPORTED_EXTENSION");
        assert!(
            !err.message.contains("accessors"),
            "should not list every accessor: {}",
            err.message
        );
        assert!(err.message.contains("MADE_UP_extension"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn draco_triangle_and_texture_transform() {
        let dir = std::env::temp_dir().join(format!("mv-draco-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut mesh = draco_core::Mesh::new();
        let mut position = draco_core::PointAttribute::new();
        position.init(
            draco_core::GeometryAttributeType::Position,
            3,
            draco_core::DataType::Float32,
            false,
            3,
        );
        let coords: [f32; 9] = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 2.0, 0.0];
        for (i, value) in coords.iter().enumerate() {
            position.buffer_mut().write(i * 4, &value.to_le_bytes());
        }
        mesh.add_attribute(position);
        mesh.set_num_points(3);
        mesh.set_num_faces(1);
        mesh.set_face(
            draco_core::FaceIndex(0),
            [0u32.into(), 1u32.into(), 2u32.into()],
        );
        let mut encoder = draco_core::MeshEncoder::new();
        encoder.set_mesh(mesh);
        let mut buffer = draco_core::EncoderBuffer::new();
        encoder
            .encode(&draco_core::EncoderOptions::new(), &mut buffer)
            .expect("encode draco triangle");
        let bytes = buffer.data().to_vec();
        let json = format!(
            r#"{{
                "asset": {{"version": "2.0"}},
                "extensionsUsed": ["KHR_draco_mesh_compression"],
                "extensionsRequired": ["KHR_draco_mesh_compression"],
                "buffers": [{{"byteLength": {len}}}],
                "bufferViews": [{{"buffer": 0, "byteOffset": 0, "byteLength": {len}}}],
                "accessors": [{{"componentType": 5126, "count": 3, "type": "VEC3", "min": [0,0,0], "max": [1,2,0]}}],
                "meshes": [{{"primitives": [{{
                    "attributes": {{"POSITION": 0}},
                    "extensions": {{"KHR_draco_mesh_compression": {{
                        "bufferView": 0,
                        "attributes": {{"POSITION": 0}}
                    }}}}
                }}]}}],
                "nodes": [{{"mesh": 0}}],
                "scenes": [{{"nodes": [0]}}],
                "scene": 0
            }}"#,
            len = bytes.len()
        );
        let path = write_glb(&dir, "tri.glb", &json, &bytes);
        let scene = load(&path).expect("draco glb");
        assert_eq!(scene.triangle_count(), 1, "{:?}", scene.problems);
        let ys: Vec<f32> = scene.meshes[0].positions.iter().map(|p| p[1]).collect();
        assert!(
            ys.iter().any(|y| (*y - 2.0).abs() < 0.05),
            "decoded positions {:?}",
            scene.meshes[0].positions
        );

        let mut bin = Vec::new();
        for v in [[0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            bin.extend(v[0].to_le_bytes());
            bin.extend(v[1].to_le_bytes());
            bin.extend(v[2].to_le_bytes());
        }
        for v in [[0f32, 0.0], [1.0, 0.0], [0.0, 1.0]] {
            bin.extend(v[0].to_le_bytes());
            bin.extend(v[1].to_le_bytes());
        }
        bin.extend(0u16.to_le_bytes());
        bin.extend(1u16.to_le_bytes());
        bin.extend(2u16.to_le_bytes());
        let json = format!(
            r#"{{
                "asset": {{"version": "2.0"}},
                "extensionsUsed": ["KHR_texture_transform"],
                "extensionsRequired": ["KHR_texture_transform"],
                "buffers": [{{"byteLength": {len}}}],
                "bufferViews": [
                    {{"buffer": 0, "byteOffset": 0, "byteLength": 36}},
                    {{"buffer": 0, "byteOffset": 36, "byteLength": 24}},
                    {{"buffer": 0, "byteOffset": 60, "byteLength": 6, "target": 34963}}
                ],
                "accessors": [
                    {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0,0,0], "max": [1,1,0]}},
                    {{"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC2"}},
                    {{"bufferView": 2, "componentType": 5123, "count": 3, "type": "SCALAR"}}
                ],
                "materials": [{{
                    "pbrMetallicRoughness": {{
                        "baseColorTexture": {{
                            "index": 0,
                            "extensions": {{
                                "KHR_texture_transform": {{"offset": [0.25, 0.0], "scale": [2.0, 1.0]}}
                            }}
                        }}
                    }}
                }}],
                "textures": [{{"source": 0}}],
                "images": [{{"uri": "missing.png"}}],
                "meshes": [{{"primitives": [{{
                    "attributes": {{"POSITION": 0, "TEXCOORD_0": 1}},
                    "indices": 2,
                    "material": 0
                }}]}}],
                "nodes": [{{"mesh": 0}}],
                "scenes": [{{"nodes": [0]}}],
                "scene": 0
            }}"#,
            len = bin.len()
        );
        let path = write_glb(&dir, "uv.glb", &json, &bin);
        let scene = load(&path).expect("texture transform");
        let uv = &scene.meshes[0].uvs[0].coords;
        assert!(
            (uv[0][0] - 0.25).abs() < 1e-4 && (uv[1][0] - 2.25).abs() < 1e-4,
            "transformed uvs {uv:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
