use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::scene::{push_checked, Material, Mesh, Node, Problem, Scene, Texture, Trs, UvSet};
use crate::MvError;

use super::textures::{self, decode_bytes};

pub fn load(path: &Path) -> Result<Scene> {
    let text = fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::InvalidData {
            MvError::parse(format!("OBJ is not valid UTF-8 ({err})"))
        } else {
            MvError::io(err)
        }
    })?;
    // Wavefront files are almost always UTF-8 or ASCII. A binary file that
    // happens to end in .obj should fail the parse rather than panic.
    if text.bytes().take(256).any(|b| b == 0) {
        return Err(MvError::parse("OBJ contains NUL bytes"));
    }
    parse_obj(path, &text)
}

struct FaceVert {
    v: i32,
    vt: Option<i32>,
    vn: Option<i32>,
}

struct RawFace {
    verts: Vec<FaceVert>,
    material: Option<String>,
    object: String,
}

struct Mtl {
    name: String,
    kd: [f32; 3],
    d: f32,
    ns: f32,
    map_kd: Option<String>,
}

pub fn parse_obj(path: &Path, text: &str) -> Result<Scene> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut faces: Vec<RawFace> = Vec::new();
    let mut material: Option<String> = None;
    let mut object = "default".to_string();
    let mut mtllibs: Vec<String> = Vec::new();
    let mut tris = 0usize;

    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = split_ws(line);
        let tag = parts.next().unwrap_or("");
        match tag {
            "v" => {
                let x = num(parts.next(), line_no, "v")?;
                let y = num(parts.next(), line_no, "v")?;
                let z = num(parts.next(), line_no, "v")?;
                positions.push([x, y, z]);
            }
            "vt" => {
                let u = num(parts.next(), line_no, "vt")?;
                let v = num(parts.next(), line_no, "vt")?;
                uvs.push([u, v]);
            }
            "vn" => {
                let x = num(parts.next(), line_no, "vn")?;
                let y = num(parts.next(), line_no, "vn")?;
                let z = num(parts.next(), line_no, "vn")?;
                normals.push([x, y, z]);
            }
            "f" => {
                let mut verts = Vec::new();
                for token in parts {
                    verts.push(parse_face_vert(token, line_no)?);
                }
                if verts.len() >= 3 {
                    let extra = verts.len() - 2;
                    for _ in 0..extra {
                        push_checked(&mut tris)?;
                    }
                    faces.push(RawFace {
                        verts,
                        material: material.clone(),
                        object: object.clone(),
                    });
                }
            }
            "usemtl" => {
                material = Some(parts.collect::<Vec<_>>().join(" "));
            }
            "o" | "g" => {
                let name = parts.collect::<Vec<_>>().join(" ");
                if !name.is_empty() {
                    object = name;
                }
            }
            "mtllib" => {
                let name = rest_of(line, "mtllib");
                if !name.is_empty() {
                    mtllibs.push(name);
                }
            }
            _ => {}
        }
    }

    let mtls = load_mtls(path, &mtllibs);
    let mut scene = Scene::new("obj");
    let mut mat_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for mtl in &mtls {
        let idx = scene.materials.len();
        let mut material = Material {
            name: mtl.name.clone(),
            base_color: [
                mtl.kd[0] * mtl.d,
                mtl.kd[1] * mtl.d,
                mtl.kd[2] * mtl.d,
                mtl.d,
            ],
            metallic: 0.0,
            roughness: (1.0 - (mtl.ns / 200.0).clamp(0.0, 1.0)).clamp(0.15, 1.0),
            base_color_texture: None,
            shading_model: "obj-mtl".into(),
            double_sided: true,
        };
        if let Some(map) = &mtl.map_kd {
            material.base_color_texture = Some(push_texture(&mut scene, path, map));
        }
        mat_index.insert(mtl.name.clone(), idx);
        scene.materials.push(material);
    }

    // Group faces by object + material.
    let mut groups: Vec<(String, Option<String>, Vec<usize>)> = Vec::new();
    for (fi, face) in faces.iter().enumerate() {
        if let Some(group) = groups
            .iter_mut()
            .find(|g| g.0 == face.object && g.1 == face.material)
        {
            group.2.push(fi);
        } else {
            groups.push((face.object.clone(), face.material.clone(), vec![fi]));
        }
    }

    scene.nodes.push(Node {
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "obj".into()),
        parent: None,
        children: Vec::new(),
        rest: Trs::identity(),
        meshes: Vec::new(),
        is_bone: false,
    });

    if groups.is_empty() {
        return Ok(scene);
    }

    for (object, material_name, face_ids) in groups {
        let mut mesh = Mesh::new(object.clone());
        if let Some(name) = &material_name {
            mesh.material = mat_index.get(name).copied();
        }
        let mut uv_coords = Vec::new();
        let use_uv = faces.iter().any(|f| f.verts.iter().any(|v| v.vt.is_some()));
        for fi in face_ids {
            let face = &faces[fi];
            let base_verts: Vec<u32> = face
                .verts
                .iter()
                .map(|fv| {
                    let vi = resolve_index(fv.v, positions.len()).unwrap_or(0);
                    let pos = positions.get(vi).copied().unwrap_or([0.0, 0.0, 0.0]);
                    let normal = fv
                        .vn
                        .and_then(|i| resolve_index(i, normals.len()))
                        .and_then(|i| normals.get(i).copied());
                    let uv = fv
                        .vt
                        .and_then(|i| resolve_index(i, uvs.len()))
                        .and_then(|i| uvs.get(i).copied());
                    let idx = mesh.positions.len() as u32;
                    mesh.positions.push(pos);
                    mesh.normals.push(normal.unwrap_or([0.0, 1.0, 0.0]));
                    if use_uv {
                        uv_coords.push(uv.unwrap_or([0.0, 0.0]));
                    }
                    idx
                })
                .collect();
            for t in 1..base_verts.len().saturating_sub(1) {
                mesh.indices
                    .extend([base_verts[0], base_verts[t], base_verts[t + 1]]);
            }
        }
        if mesh.normals.iter().all(|n| *n == [0.0, 1.0, 0.0]) {
            generate_normals(&mut mesh);
        }
        if use_uv {
            mesh.uvs.push(UvSet {
                name: "UVMap".into(),
                coords: uv_coords,
            });
        }
        let mi = scene.meshes.len();
        scene.meshes.push(mesh);
        let node_index = scene.nodes.len();
        scene.nodes.push(Node {
            name: object,
            parent: Some(0),
            children: Vec::new(),
            rest: Trs::identity(),
            meshes: vec![mi],
            is_bone: false,
        });
        scene.nodes[0].children.push(node_index);
        let _ = material_name;
    }
    Ok(scene)
}

fn push_texture(scene: &mut Scene, model: &Path, referenced: &str) -> usize {
    let found = textures::resolve(model, referenced);
    let mut texture = Texture {
        name: Path::new(referenced)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| referenced.to_string()),
        referenced_path: referenced.to_string(),
        resolved_path: found.resolved.as_ref().map(|p| p.display().to_string()),
        embedded: false,
        status: "missing".into(),
        searched: found.searched.clone(),
        width: 0,
        height: 0,
        rgba: Vec::new(),
    };
    if found.absolute_reference {
        scene.add_problem(Problem::warning(
            "TEXTURE_ABSOLUTE_PATH",
            format!("texture '{referenced}' uses an absolute path"),
        ));
    }
    if let Some(path) = &found.resolved {
        match fs::read(path) {
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
                        format!("could not decode texture {}", path.display()),
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
        problem.path = Some(referenced.to_string());
        problem.searched = found.searched;
        scene.add_problem(problem);
    }
    let idx = scene.textures.len();
    scene.textures.push(texture);
    idx
}

fn load_mtls(obj_path: &Path, names: &[String]) -> Vec<Mtl> {
    let mut out = Vec::new();
    let dir = obj_path.parent().unwrap_or_else(|| Path::new("."));
    for name in names {
        let path = dir.join(name);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut current: Option<Mtl> = None;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = split_ws(line);
            let tag = parts.next().unwrap_or("");
            match tag {
                "newmtl" => {
                    if let Some(m) = current.take() {
                        out.push(m);
                    }
                    current = Some(Mtl {
                        name: parts.collect::<Vec<_>>().join(" "),
                        kd: [0.75, 0.75, 0.75],
                        d: 1.0,
                        ns: 32.0,
                        map_kd: None,
                    });
                }
                "Kd" => {
                    if let Some(m) = current.as_mut() {
                        m.kd = [
                            parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0),
                            parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0),
                            parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0),
                        ];
                    }
                }
                "d" | "Tr" => {
                    if let Some(m) = current.as_mut() {
                        if let Some(v) = parts.next().and_then(|s| s.parse().ok()) {
                            m.d = if tag == "Tr" { 1.0 - v } else { v };
                        }
                    }
                }
                "Ns" => {
                    if let Some(m) = current.as_mut() {
                        if let Some(v) = parts.next().and_then(|s| s.parse().ok()) {
                            m.ns = v;
                        }
                    }
                }
                "map_Kd" => {
                    if let Some(m) = current.as_mut() {
                        m.map_kd = Some(map_path(line));
                    }
                }
                _ => {}
            }
        }
        if let Some(m) = current.take() {
            out.push(m);
        }
    }
    out
}

fn map_path(line: &str) -> String {
    let rest = rest_of(line, "map_Kd");
    let mut last = String::new();
    for token in split_ws(&rest) {
        if token.starts_with('-') {
            continue;
        }
        last = token.to_string();
    }
    last
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

fn parse_face_vert(token: &str, line_no: usize) -> Result<FaceVert> {
    let mut bits = token.split('/');
    let v: i32 = bits
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| MvError::parse(format!("bad face index on line {}", line_no + 1)))?;
    let vt = bits
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| MvError::parse(format!("bad UV index on line {}", line_no + 1)))?;
    let vn = bits
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| MvError::parse(format!("bad normal index on line {}", line_no + 1)))?;
    Ok(FaceVert { v, vt, vn })
}

fn resolve_index(index: i32, len: usize) -> Option<usize> {
    if index > 0 {
        let u = index as usize - 1;
        (u < len).then_some(u)
    } else if index < 0 {
        let u = len as i32 + index;
        (u >= 0).then_some(u as usize)
    } else {
        None
    }
}

fn num(token: Option<&str>, line_no: usize, kind: &str) -> Result<f32> {
    token
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| MvError::parse(format!("bad {kind} on line {}", line_no + 1)))
}

fn rest_of(line: &str, tag: &str) -> String {
    line.trim()
        .strip_prefix(tag)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn split_ws(line: &str) -> impl Iterator<Item = &str> {
    line.split_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangulates_a_quad_and_reads_mtl() {
        let dir = std::env::temp_dir().join(format!("modelview-obj-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("textures")).unwrap();
        fs::write(
            dir.join("textures").join("checker.png"),
            include_bytes_or_empty(),
        )
        .unwrap();
        let obj = dir.join("cube.obj");
        fs::write(
            &obj,
            "mtllib cube.mtl\no Box\nusemtl red\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvn 0 0 1\nf 1//1 2//1 3//1 4//1\n",
        )
        .unwrap();
        fs::write(&dir.join("cube.mtl"), "newmtl red\nKd 1 0 0\nNs 10\n").unwrap();
        let scene = load(&obj).unwrap();
        assert_eq!(scene.triangle_count(), 2);
        assert_eq!(scene.materials[0].name, "red");
        assert!(scene.materials[0].base_color[0] > 0.9);
        let _ = fs::remove_dir_all(&dir);
    }

    fn include_bytes_or_empty() -> &'static [u8] {
        b"not a png"
    }
}
