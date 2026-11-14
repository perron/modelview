use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::scene::{push_checked, Mesh, Node, Scene, Trs};
use crate::MvError;

pub fn load(path: &Path) -> Result<Scene> {
    let bytes = fs::read(path).map_err(MvError::io)?;
    let mut scene = Scene::new("stl");
    let triangles = if looks_ascii(&bytes) {
        match parse_ascii(&bytes) {
            Ok(tris) if !tris.is_empty() => tris,
            Ok(_) | Err(_) if binary_size_matches(&bytes) => parse_binary(&bytes)?,
            Ok(tris) => tris,
            Err(err) => return Err(err),
        }
    } else {
        parse_binary(&bytes)?
    };

    let mut mesh = Mesh::new(
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "stl".into()),
    );
    let mut count = 0usize;
    for (normal, verts) in triangles {
        push_checked(&mut count)?;
        let base = mesh.positions.len() as u32;
        for v in verts {
            mesh.positions.push(v);
            mesh.normals.push(normal);
        }
        mesh.indices.extend([base, base + 1, base + 2]);
    }
    scene.nodes.push(Node {
        name: mesh.name.clone(),
        parent: None,
        children: Vec::new(),
        rest: Trs::identity(),
        meshes: vec![0],
        is_bone: false,
    });
    scene.meshes.push(mesh);
    scene.add_problem(crate::scene::Problem::info(
        "UNITS_UNKNOWN",
        "STL has no unit or axis metadata; coordinates are shown as stored, treated as meters",
    ));
    Ok(scene)
}

fn looks_ascii(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(5)];
    if !head.eq_ignore_ascii_case(b"solid") {
        return false;
    }
    let window = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(window);
    text.to_ascii_lowercase().contains("facet")
}

fn binary_size_matches(bytes: &[u8]) -> bool {
    if bytes.len() < 84 {
        return false;
    }
    let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
    bytes.len() == 84 + count * 50
}

fn parse_ascii(bytes: &[u8]) -> Result<Vec<([f32; 3], [[f32; 3]; 3])>> {
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| MvError::parse("ASCII STL is not valid UTF-8"))?;
    let mut out = Vec::new();
    let mut normal = [0.0, 0.0, 1.0];
    let mut verts: Vec<[f32; 3]> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("facet normal") {
            normal = parse3(&lower["facet normal".len()..]).unwrap_or([0.0, 0.0, 1.0]);
            verts.clear();
        } else if lower.starts_with("vertex") {
            if let Some(v) = parse3(&lower["vertex".len()..]) {
                verts.push(v);
            }
        } else if lower.starts_with("endfacet") {
            if verts.len() >= 3 {
                out.push((normal, [verts[0], verts[1], verts[2]]));
            }
            verts.clear();
        }
    }
    if out.is_empty() && text.to_ascii_lowercase().contains("facet") {
        return Err(MvError::parse("ASCII STL contained no triangles"));
    }
    Ok(out)
}

fn parse3(text: &str) -> Option<[f32; 3]> {
    let mut it = text.split_whitespace();
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    Some([x, y, z])
}

fn parse_binary(bytes: &[u8]) -> Result<Vec<([f32; 3], [[f32; 3]; 3])>> {
    if bytes.len() < 84 {
        return Err(MvError::parse(
            "binary STL is shorter than the 84-byte header",
        ));
    }
    let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
    if count > crate::scene::MAX_TRIANGLES {
        return Err(MvError::parse(format!(
            "binary STL declares {count} triangles"
        )));
    }
    let need = 84usize.saturating_add(count.saturating_mul(50));
    if bytes.len() < need {
        return Err(MvError::parse(format!(
            "binary STL declares {count} triangles but the file is too short"
        )));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = 84 + i * 50;
        let normal = read_f32_3(bytes, o);
        let a = read_f32_3(bytes, o + 12);
        let b = read_f32_3(bytes, o + 24);
        let c = read_f32_3(bytes, o + 36);
        out.push((normal, [a, b, c]));
    }
    Ok(out)
}

fn read_f32_3(bytes: &[u8], offset: usize) -> [f32; 3] {
    [
        f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()),
        f32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()),
        f32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_binary_cube_have_twelve_triangles() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/generated");
        let ascii = load(&root.join("cube_ascii.stl")).unwrap();
        let binary = load(&root.join("cube_binary.stl")).unwrap();
        assert_eq!(ascii.triangle_count(), 12);
        assert_eq!(binary.triangle_count(), 12);
        assert!(ascii.problems.iter().any(|p| p.code == "UNITS_UNKNOWN"));
    }
}
