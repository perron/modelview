use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::scene::{push_checked, Mesh, Node, Problem, Scene, Trs, UvSet};
use crate::MvError;

pub fn load(path: &Path) -> Result<Scene> {
    let bytes = fs::read(path).map_err(MvError::io)?;
    let text_head = String::from_utf8_lossy(&bytes[..bytes.len().min(16)]);
    if !text_head
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("ply")
    {
        return Err(MvError::parse("PLY file does not start with 'ply'"));
    }
    let header_end =
        find_header_end(&bytes).ok_or_else(|| MvError::parse("PLY header has no end_header"))?;
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| MvError::parse("PLY header is not UTF-8"))?;
    let body = &bytes[header_end..];
    let parsed = parse_header(header)?;
    let mut scene = Scene::new("ply");
    let mesh = read_body(&parsed, body)?;
    scene.add_problem(Problem::info(
        "UNITS_UNKNOWN",
        "PLY has no unit or axis metadata; coordinates are shown as stored, treated as meters",
    ));
    scene.nodes.push(Node {
        name: mesh.name.clone(),
        parent: None,
        children: Vec::new(),
        rest: Trs::identity(),
        meshes: vec![0],
        is_bone: false,
    });
    scene.meshes.push(mesh);
    Ok(scene)
}

struct Header {
    format: Format,
    vertex_count: usize,
    face_count: usize,
    vertex_props: Vec<Prop>,
    face_props: Vec<Prop>,
}

enum Format {
    Ascii,
    BinaryLe,
    BinaryBe,
}

struct Prop {
    name: String,
    kind: Kind,
    list_count: Option<Kind>,
}

#[derive(Clone, Copy)]
enum Kind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

fn parse_header(header: &str) -> Result<Header> {
    let mut format = None;
    let mut vertex_count = 0usize;
    let mut face_count = 0usize;
    let mut element = "";
    let mut vertex_props = Vec::new();
    let mut face_props = Vec::new();
    for raw in header.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("comment") || line == "ply" || line == "end_header" {
            continue;
        }
        let mut parts = line.split_whitespace();
        let tag = parts.next().unwrap_or("");
        match tag {
            "format" => {
                format = Some(match parts.next().unwrap_or("") {
                    "ascii" => Format::Ascii,
                    "binary_little_endian" => Format::BinaryLe,
                    "binary_big_endian" => Format::BinaryBe,
                    other => {
                        return Err(MvError::parse(format!("unsupported PLY format '{other}'")));
                    }
                });
            }
            "element" => {
                element = match parts.next().unwrap_or("") {
                    "vertex" => {
                        vertex_count = parts
                            .next()
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| MvError::parse("bad vertex count"))?;
                        "vertex"
                    }
                    "face" => {
                        face_count = parts
                            .next()
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| MvError::parse("bad face count"))?;
                        "face"
                    }
                    _ => "other",
                };
            }
            "property" => {
                let prop = parse_property(parts)?;
                match element {
                    "vertex" => vertex_props.push(prop),
                    "face" => face_props.push(prop),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if vertex_count > crate::scene::MAX_TRIANGLES * 3 || face_count > crate::scene::MAX_TRIANGLES {
        return Err(MvError::parse("PLY element count is unreasonably large"));
    }
    if !vertex_props.iter().any(|p| p.name == "x") {
        return Err(MvError::parse("PLY vertices have no x property"));
    }
    Ok(Header {
        format: format.ok_or_else(|| MvError::parse("PLY header has no format"))?,
        vertex_count,
        face_count,
        vertex_props,
        face_props,
    })
}

fn parse_property<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<Prop> {
    let first = parts.next().unwrap_or("");
    if first == "list" {
        let count = kind(parts.next().unwrap_or(""))?;
        let value = kind(parts.next().unwrap_or(""))?;
        let name = parts.next().unwrap_or("list").to_string();
        return Ok(Prop {
            name,
            kind: value,
            list_count: Some(count),
        });
    }
    Ok(Prop {
        name: parts.next().unwrap_or("unnamed").to_string(),
        kind: kind(first)?,
        list_count: None,
    })
}

fn kind(name: &str) -> Result<Kind> {
    Ok(match name {
        "char" | "int8" => Kind::I8,
        "uchar" | "uint8" => Kind::U8,
        "short" | "int16" => Kind::I16,
        "ushort" | "uint16" => Kind::U16,
        "int" | "int32" => Kind::I32,
        "uint" | "uint32" => Kind::U32,
        "float" | "float32" => Kind::F32,
        "double" | "float64" => Kind::F64,
        other => return Err(MvError::parse(format!("unsupported PLY type '{other}'"))),
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    ascii: Option<Vec<String>>,
    ascii_i: usize,
    be: bool,
}

impl<'a> Cursor<'a> {
    fn next_f32(&mut self, kind: Kind) -> Result<f32> {
        Ok(self.next_f64(kind)? as f32)
    }

    fn next_f64(&mut self, kind: Kind) -> Result<f64> {
        if let Some(tokens) = &self.ascii {
            let token = tokens
                .get(self.ascii_i)
                .ok_or_else(|| MvError::parse("PLY ascii body ended early"))?;
            self.ascii_i += 1;
            return token
                .parse::<f64>()
                .map_err(|_| MvError::parse(format!("bad PLY number '{token}'")));
        }
        let n = kind_size(kind);
        if self.bytes.len() < n {
            return Err(MvError::parse("PLY binary body ended early"));
        }
        let raw = &self.bytes[..n];
        self.bytes = &self.bytes[n..];
        Ok(read_num(raw, kind, self.be))
    }

    fn next_usize(&mut self, kind: Kind) -> Result<usize> {
        let v = self.next_f64(kind)?;
        if v < 0.0 {
            return Err(MvError::parse("negative PLY list count"));
        }
        Ok(v as usize)
    }
}

fn read_body(header: &Header, body: &[u8]) -> Result<Mesh> {
    let mut cursor = match header.format {
        Format::Ascii => Cursor {
            bytes: b"",
            ascii: Some(
                String::from_utf8_lossy(body)
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ascii_i: 0,
            be: false,
        },
        Format::BinaryLe => Cursor {
            bytes: body,
            ascii: None,
            ascii_i: 0,
            be: false,
        },
        Format::BinaryBe => Cursor {
            bytes: body,
            ascii: None,
            ascii_i: 0,
            be: true,
        },
    };

    let xi = prop_index(&header.vertex_props, "x");
    let yi = prop_index(&header.vertex_props, "y");
    let zi = prop_index(&header.vertex_props, "z");
    let nxi = prop_index_opt(&header.vertex_props, "nx");
    let nyi = prop_index_opt(&header.vertex_props, "ny");
    let nzi = prop_index_opt(&header.vertex_props, "nz");
    let ui = prop_index_opt(&header.vertex_props, "s")
        .or_else(|| prop_index_opt(&header.vertex_props, "u"))
        .or_else(|| prop_index_opt(&header.vertex_props, "texture_u"));
    let vi = prop_index_opt(&header.vertex_props, "t")
        .or_else(|| prop_index_opt(&header.vertex_props, "v"))
        .or_else(|| prop_index_opt(&header.vertex_props, "texture_v"));

    let mut positions = Vec::with_capacity(header.vertex_count);
    let mut normals = Vec::with_capacity(header.vertex_count);
    let mut uv_coords = Vec::new();
    let mut saw_normal = false;
    for _ in 0..header.vertex_count {
        let mut values = Vec::with_capacity(header.vertex_props.len());
        for prop in &header.vertex_props {
            if prop.list_count.is_some() {
                return Err(MvError::parse(
                    "list properties on PLY vertices are not supported",
                ));
            }
            values.push(cursor.next_f32(prop.kind)?);
        }
        let get = |i: usize| values.get(i).copied().unwrap_or(0.0);
        positions.push([get(xi), get(yi), get(zi)]);
        if let (Some(a), Some(b), Some(c)) = (nxi, nyi, nzi) {
            saw_normal = true;
            normals.push([get(a), get(b), get(c)]);
        } else {
            normals.push([0.0, 1.0, 0.0]);
        }
        if let (Some(u), Some(v)) = (ui, vi) {
            uv_coords.push([get(u), get(v)]);
        }
    }

    let mut mesh = Mesh::new("ply");
    mesh.positions = positions;
    mesh.normals = normals;
    if !uv_coords.is_empty() {
        mesh.uvs.push(UvSet {
            name: "UVMap".into(),
            coords: uv_coords,
        });
    }

    let list_prop = header
        .face_props
        .iter()
        .find(|p| p.list_count.is_some())
        .ok_or_else(|| MvError::parse("PLY has no face index list"))?;
    let count_kind = list_prop.list_count.unwrap();
    let index_kind = list_prop.kind;
    // Consume any properties before the list, and ignore ones after, per face.
    let list_at = header
        .face_props
        .iter()
        .position(|p| p.list_count.is_some())
        .unwrap();

    let mut tris = 0usize;
    for _ in 0..header.face_count {
        for prop in header.face_props.iter().take(list_at) {
            let _ = cursor.next_f32(prop.kind)?;
        }
        let n = cursor.next_usize(count_kind)?;
        if n > 64 {
            return Err(MvError::parse(format!("PLY face has {n} vertices")));
        }
        let mut idx = Vec::with_capacity(n);
        for _ in 0..n {
            let i = cursor.next_usize(index_kind)?;
            if i >= mesh.positions.len() {
                return Err(MvError::parse("PLY face index is out of range"));
            }
            idx.push(i as u32);
        }
        for prop in header.face_props.iter().skip(list_at + 1) {
            if prop.list_count.is_some() {
                let extra = cursor.next_usize(prop.list_count.unwrap())?;
                for _ in 0..extra {
                    let _ = cursor.next_f32(prop.kind)?;
                }
            } else {
                let _ = cursor.next_f32(prop.kind)?;
            }
        }
        if idx.len() >= 3 {
            for t in 1..idx.len() - 1 {
                push_checked(&mut tris)?;
                mesh.indices.extend([idx[0], idx[t], idx[t + 1]]);
            }
        }
    }
    if !saw_normal {
        generate_flat(&mut mesh);
    }
    Ok(mesh)
}

fn generate_flat(mesh: &mut Mesh) {
    let mut acc = vec![glam::Vec3::ZERO; mesh.positions.len()];
    for tri in mesh.indices.chunks_exact(3) {
        let a = glam::Vec3::from(mesh.positions[tri[0] as usize]);
        let b = glam::Vec3::from(mesh.positions[tri[1] as usize]);
        let c = glam::Vec3::from(mesh.positions[tri[2] as usize]);
        let n = (b - a).cross(c - a);
        acc[tri[0] as usize] += n;
        acc[tri[1] as usize] += n;
        acc[tri[2] as usize] += n;
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

fn prop_index(props: &[Prop], name: &str) -> usize {
    props.iter().position(|p| p.name == name).unwrap_or(0)
}

fn prop_index_opt(props: &[Prop], name: &str) -> Option<usize> {
    props.iter().position(|p| p.name == name)
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    let marker = b"end_header";
    let pos = bytes.windows(marker.len()).position(|w| w == marker)?;
    let mut end = pos + marker.len();
    if bytes.get(end) == Some(&b'\r') {
        end += 1;
    }
    if bytes.get(end) == Some(&b'\n') {
        end += 1;
    }
    Some(end)
}

fn kind_size(kind: Kind) -> usize {
    match kind {
        Kind::I8 | Kind::U8 => 1,
        Kind::I16 | Kind::U16 => 2,
        Kind::I32 | Kind::U32 | Kind::F32 => 4,
        Kind::F64 => 8,
    }
}

fn read_num(raw: &[u8], kind: Kind, be: bool) -> f64 {
    match kind {
        Kind::I8 => raw[0] as i8 as f64,
        Kind::U8 => raw[0] as f64,
        Kind::I16 => i16_of(raw, be) as f64,
        Kind::U16 => u16_of(raw, be) as f64,
        Kind::I32 => i32_of(raw, be) as f64,
        Kind::U32 => u32_of(raw, be) as f64,
        Kind::F32 => f32_of(raw, be) as f64,
        Kind::F64 => f64_of(raw, be),
    }
}

fn i16_of(b: &[u8], be: bool) -> i16 {
    let u = if be {
        u16::from_be_bytes([b[0], b[1]])
    } else {
        u16::from_le_bytes([b[0], b[1]])
    };
    u as i16
}
fn u16_of(b: &[u8], be: bool) -> u16 {
    if be {
        u16::from_be_bytes([b[0], b[1]])
    } else {
        u16::from_le_bytes([b[0], b[1]])
    }
}
fn i32_of(b: &[u8], be: bool) -> i32 {
    let u = if be {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };
    u as i32
}
fn u32_of(b: &[u8], be: bool) -> u32 {
    if be {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
}
fn f32_of(b: &[u8], be: bool) -> f32 {
    f32::from_bits(u32_of(b, be))
}
fn f64_of(b: &[u8], be: bool) -> f64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&b[..8]);
    if be {
        f64::from_be_bytes(buf)
    } else {
        f64::from_le_bytes(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_binary_square() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/generated");
        let ascii = load(&root.join("square_ascii.ply")).unwrap();
        let binary = load(&root.join("square_binary.ply")).unwrap();
        assert_eq!(ascii.triangle_count(), 2);
        assert_eq!(binary.triangle_count(), 2);
        assert_eq!(ascii.vertex_count(), 4);
        assert!(ascii.meshes[0].normals[0][2] > 0.9);
    }
}
