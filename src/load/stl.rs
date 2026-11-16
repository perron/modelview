use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::scene::{push_checked, Mesh, Node, Scene, Trs};
use crate::MvError;

pub fn load(path: &Path) -> Result<Scene> {
    let bytes = fs::read(path).map_err(MvError::io)?;
    let mut scene = Scene::new("stl");
    let mut mesh = Mesh::new(
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "stl".into()),
    );
    if looks_ascii(&bytes) && !binary_size_matches(&bytes) {
        let triangles = parse_ascii(&bytes)?;
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
    } else {
        // Binary records are streamed straight into the mesh. A vertex that
        // repeats with the same face normal is stored once.
        fill_binary(&mut mesh, &bytes)?;
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

/// A binary STL is `84 + 50 * n` bytes. Any `n` that makes that identity
/// true is accepted. A count that does not match the length is corrupt,
/// and is rejected before any triangle storage is allocated.
fn fill_binary(mesh: &mut Mesh, bytes: &[u8]) -> Result<()> {
    if bytes.len() < 84 {
        return Err(MvError::parse(
            "binary STL is shorter than the 84-byte header",
        ));
    }
    let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
    let expected = count.checked_mul(50).and_then(|n| n.checked_add(84));
    if expected != Some(bytes.len()) {
        return Err(MvError::parse(format!(
            "binary STL declares {count} triangles but the file is {} bytes, not 84 + 50 * {count}",
            bytes.len()
        )));
    }
    mesh.indices.reserve(count.saturating_mul(3));
    let mut weld: HashMap<[u32; 6], u32> = HashMap::new();
    for i in 0..count {
        let o = 84 + i * 50;
        let normal = read_f32_3(bytes, o);
        for v in [
            read_f32_3(bytes, o + 12),
            read_f32_3(bytes, o + 24),
            read_f32_3(bytes, o + 36),
        ] {
            let key = [
                v[0].to_bits(),
                v[1].to_bits(),
                v[2].to_bits(),
                normal[0].to_bits(),
                normal[1].to_bits(),
                normal[2].to_bits(),
            ];
            let idx = if let Some(&idx) = weld.get(&key) {
                idx
            } else {
                let idx = mesh.positions.len() as u32;
                mesh.positions.push(v);
                mesh.normals.push(normal);
                weld.insert(key, idx);
                idx
            };
            mesh.indices.push(idx);
        }
    }
    Ok(())
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

    #[test]
    fn binary_stl_larger_than_the_old_cap_loads_without_duplicating_vertices() {
        use std::io::Write;
        // Just past the old 5_000_000 limit. Every record is the same
        // triangle, so a correct weld keeps three vertices, not 15 million.
        let count: u32 = 5_000_001;
        let dir = std::env::temp_dir().join(format!("mv-stl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.stl");
        let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
        file.write_all(&[0u8; 80]).unwrap();
        file.write_all(&count.to_le_bytes()).unwrap();
        let mut rec = [0u8; 50];
        rec[8..12].copy_from_slice(&1f32.to_le_bytes());
        rec[24..28].copy_from_slice(&1f32.to_le_bytes());
        rec[40..44].copy_from_slice(&1f32.to_le_bytes());
        let chunk = rec.repeat(1000);
        let full = (count as usize) / 1000;
        let rem = (count as usize) % 1000;
        for _ in 0..full {
            file.write_all(&chunk).unwrap();
        }
        file.write_all(&rec[..rem * 50]).unwrap();
        file.flush().unwrap();
        drop(file);
        let len = fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(len, 84 + 50 * count as usize);
        let scene = load(&path).unwrap();
        assert_eq!(scene.triangle_count(), count as usize);
        assert_eq!(
            scene.vertex_count(),
            3,
            "repeated STL corners should be stored once"
        );

        let short = dir.join("short.stl");
        let mut bad = vec![0u8; 84 + 50];
        bad[80..84].copy_from_slice(&2u32.to_le_bytes());
        fs::write(&short, &bad).unwrap();
        let err = load(&short).unwrap_err();
        assert_eq!(err.code, "PARSE_FAILED");
        assert!(
            err.message.contains("declares 2 triangles"),
            "{}",
            err.message
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
