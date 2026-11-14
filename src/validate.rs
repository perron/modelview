use crate::scene::{Problem, Scene};
use glam::Vec3;

/// Geometry checks shared by every loader. Loaders add format-specific problems
/// themselves (missing textures, units, unsupported features).
pub fn add_geometry_problems(scene: &mut Scene) {
    let mut zero_area = 0u32;
    let mut total = 0u32;
    let mut meshes_missing_uv = 0u32;
    let mut extra = Vec::new();
    for mesh in &scene.meshes {
        let has_uv = mesh
            .uvs
            .iter()
            .any(|set| set.coords.len() == mesh.positions.len() && !set.coords.is_empty());
        if mesh.indices.len() >= 3 && !has_uv {
            meshes_missing_uv += 1;
        }
        for tri in mesh.indices.chunks_exact(3) {
            total += 1;
            let Some(a) = mesh.positions.get(tri[0] as usize) else {
                continue;
            };
            let Some(b) = mesh.positions.get(tri[1] as usize) else {
                continue;
            };
            let Some(c) = mesh.positions.get(tri[2] as usize) else {
                continue;
            };
            let area = (Vec3::from(*b) - Vec3::from(*a))
                .cross(Vec3::from(*c) - Vec3::from(*a))
                .length()
                * 0.5;
            if area < 1e-12 {
                zero_area += 1;
            }
        }
        if let Some(skin) = mesh.skin {
            if mesh.max_influences() > 4 {
                let name = mesh.name.clone();
                extra.push(Problem::warning(
                    "MAX_INFLUENCES_GT_4",
                    format!("mesh '{name}' has more than 4 bone influences; extra influences were dropped and the kept weights renormalized"),
                ));
            }
            if scene
                .skins
                .get(skin)
                .map(|s| s.joints.is_empty())
                .unwrap_or(true)
            {
                extra.push(Problem::warning(
                    "UNSUPPORTED_FEATURE",
                    format!("mesh '{}' references a skin with no joints", mesh.name),
                ));
            }
        }
    }
    if zero_area > 0 {
        let severity_msg = format!("{zero_area} of {total} triangles have zero area");
        if total > 0 && zero_area == total {
            scene.add_problem(Problem::error("DEGENERATE_GEOMETRY", severity_msg));
        } else {
            scene.add_problem(Problem::warning("DEGENERATE_GEOMETRY", severity_msg));
        }
    }
    if meshes_missing_uv > 0 {
        scene.add_problem(Problem::info(
            "MISSING_UV",
            format!("{meshes_missing_uv} mesh(es) have no UV set; UV-checker view will be flat"),
        ));
    }
    if scene.meshes.is_empty() || scene.triangle_count() == 0 {
        scene.add_problem(Problem::warning("EMPTY_MESH", "no triangles were loaded"));
    }

    for node in &scene.nodes {
        if node.rest.scale.iter().any(|s| *s < 0.0) {
            let name = node.name.clone();
            extra.push(Problem::warning(
                "NEGATIVE_SCALE",
                format!("node '{name}' has a negative scale"),
            ));
        }
        if node.is_bone {
            let s = node.rest.scale;
            let max = s[0].abs().max(s[1].abs()).max(s[2].abs());
            let min = s[0].abs().min(s[1].abs()).min(s[2].abs());
            if max > 1e-6 && (max - min) / max > 0.05 {
                let name = node.name.clone();
                extra.push(Problem::info(
                    "NONUNIFORM_SCALE_ON_BONE",
                    format!("bone '{name}' has non-uniform scale; skinning uses linear blend"),
                ));
            }
        }
    }
    for problem in extra {
        scene.add_problem(problem);
    }
    flag_mesh_skeleton_mismatch(scene);
}

/// A skinned scene only a couple of centimetres tall (or tens of metres tall)
/// is reported, not rescaled. BG3 and some rigged exports really are that
/// size once the file's centimetre unit is applied, and they match their
/// skeleton and ufbx's own skinned positions.
/// `100.4` prints as `100`, and a ratio of about `0.01` is never printed as `0`.
fn format_ratio(ratio: f32) -> String {
    let ratio = if ratio.is_finite() {
        ratio.max(0.0)
    } else {
        0.0
    };
    if ratio >= 10.0 {
        format!("{ratio:.0}")
    } else {
        format!("{ratio:.1}")
    }
}

pub fn flag_unusual_scale(scene: &mut Scene) {
    if scene.bone_count() == 0 || !scene.bounds.is_valid() {
        return;
    }
    let extent = scene.bounds.max_extent();
    if extent < 0.05 || extent > 100.0 {
        scene.add_problem(Problem::info(
            "UNUSUAL_SCALE",
            format!(
                "skinned scene is {extent:.4} m across after converting the file's units to meters. That is the size in the file; it was not rescaled"
            ),
        ));
    }
}

/// A skinned mesh that is tens of times smaller or larger than the skeleton
/// is the centimeter-bind failure: the bones are full size and the mesh is a
/// blob at the root. Accessories that are merely smaller than the body stay
/// under the ratio.
fn flag_mesh_skeleton_mismatch(scene: &mut Scene) {
    let worlds = scene.world_matrices(None, 0.0);
    let mut skeleton = crate::scene::Aabb::empty();
    for (i, node) in scene.nodes.iter().enumerate() {
        if node.is_bone {
            skeleton.include(worlds[i].w_axis.truncate().to_array());
        }
    }
    let skeleton_extent = skeleton.max_extent();
    if skeleton_extent < 1e-3 {
        return;
    }
    let skinned: Vec<usize> = scene
        .meshes
        .iter()
        .enumerate()
        .filter(|(_, mesh)| mesh.skin.is_some() && !mesh.joints.is_empty())
        .map(|(i, _)| i)
        .collect();
    let mut largest_posed = 0.0f32;
    let mut largest_name = String::new();
    for mi in skinned {
        let mesh = &scene.meshes[mi];
        let mut raw = crate::scene::Aabb::empty();
        for p in &mesh.positions {
            raw.include(*p);
        }
        let raw_extent = raw.max_extent();
        let mut posed_box = crate::scene::Aabb::empty();
        let (positions, _) = scene.posed_mesh(mi, &worlds);
        for p in positions {
            posed_box.include(p);
        }
        let posed = posed_box.max_extent();
        if posed < 1e-8 {
            continue;
        }
        if posed > largest_posed {
            largest_posed = posed;
            largest_name = mesh.name.clone();
        }
        // Rest-pose vertices are already in meters. Skinning should not change
        // a mesh's size by an order of magnitude. This catches a collapsed
        // body even when the skeleton measurement is collapsed with it.
        let against_vertices = if raw_extent > 1e-4 {
            raw_extent / posed
        } else {
            1.0
        };
        let grown = posed / raw_extent.max(1e-8);
        if against_vertices >= 8.0 || grown >= 8.0 {
            let ratio = against_vertices.max(grown);
            scene.add_problem(Problem::warning(
                "MESH_SKELETON_MISMATCH",
                format!(
                    "skinned mesh '{}' is {posed:.4} m across but its bind-pose vertices are {raw_extent:.4} m ({}x). The bind matrix and the bone transforms are not in the same scale",
                    mesh.name,
                    format_ratio(ratio),
                ),
            ));
        }
    }
    // Only the largest skinned mesh is compared with the skeleton. A 14cm
    // eye or tooth next to a 2m skeleton is not a scale bug.
    if skeleton_extent >= 0.25 && largest_posed > 1e-8 && skeleton_extent / largest_posed >= 12.0 {
        let ratio = skeleton_extent / largest_posed;
        scene.add_problem(Problem::warning(
            "MESH_SKELETON_MISMATCH",
            format!(
                "skinned mesh '{largest_name}' is {largest_posed:.4} m across and the skeleton is {skeleton_extent:.4} m ({}x). The bind matrix and the bone transforms are not in the same scale",
                format_ratio(ratio),
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Mesh, Node, Skin, Trs};

    #[test]
    fn flags_a_mesh_that_is_tiny_next_to_its_skeleton() {
        let mut scene = Scene::new("fbx");
        scene.nodes.push(Node {
            name: "hips".into(),
            parent: None,
            children: vec![1],
            rest: Trs::identity(),
            meshes: vec![0],
            is_bone: true,
        });
        let mut head = Trs::identity();
        head.translation = [0.0, 1.7, 0.0];
        scene.nodes.push(Node {
            name: "head".into(),
            parent: Some(0),
            children: vec![],
            rest: head,
            meshes: vec![],
            is_bone: true,
        });
        let mut mesh = Mesh::new("body");
        mesh.positions = vec![[0.0, 0.0, 0.0], [0.0, 0.018, 0.0], [0.01, 0.0, 0.0]];
        mesh.indices = vec![0, 1, 2];
        mesh.joints = vec![[0, 0, 0, 0]; 3];
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
        mesh.skin = Some(0);
        scene.meshes.push(mesh);
        scene.skins.push(Skin {
            name: "skin".into(),
            joints: vec![0],
            inverse_bind: vec![glam::Mat4::IDENTITY.to_cols_array()],
        });
        add_geometry_problems(&mut scene);
        assert!(
            scene
                .problems
                .iter()
                .any(|p| p.code == "MESH_SKELETON_MISMATCH"),
            "{:?}",
            scene.problems
        );
    }

    #[test]
    fn a_hundredfold_mismatch_does_not_print_as_zero() {
        let mut scene = Scene::new("fbx");
        scene.nodes.push(Node {
            name: "hips".into(),
            parent: None,
            children: vec![1],
            rest: Trs::identity(),
            meshes: vec![0],
            is_bone: true,
        });
        let mut head = Trs::identity();
        head.translation = [0.0, 1.7, 0.0];
        scene.nodes.push(Node {
            name: "head".into(),
            parent: Some(0),
            children: vec![],
            rest: head,
            meshes: vec![],
            is_bone: true,
        });
        let mut mesh = Mesh::new("V4_Spikes2_Crest");
        mesh.positions = vec![[0.0, 0.0, 0.0], [0.0028, 0.0, 0.0], [0.0, 0.0014, 0.0]];
        mesh.indices = vec![0, 1, 2];
        mesh.joints = vec![[0, 0, 0, 0]; 3];
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
        mesh.skin = Some(0);
        scene.meshes.push(mesh);
        let scale = glam::Mat4::from_scale(glam::Vec3::splat(100.0)).to_cols_array();
        scene.skins.push(Skin {
            name: "skin".into(),
            joints: vec![0],
            inverse_bind: vec![scale],
        });
        add_geometry_problems(&mut scene);
        let message = scene
            .problems
            .iter()
            .find(|p| p.code == "MESH_SKELETON_MISMATCH" && p.message.contains("bind-pose"))
            .map(|p| p.message.as_str())
            .unwrap_or("");
        assert!(
            message.contains("100x"),
            "ratio was rounded away: {message}"
        );
        assert!(!message.contains("(0x)"), "{message}");
    }
}

pub fn counts(scene: &Scene) -> (usize, usize, usize) {
    let errors = scene
        .problems
        .iter()
        .filter(|p| p.severity == "error")
        .count();
    let warnings = scene
        .problems
        .iter()
        .filter(|p| p.severity == "warning")
        .count();
    let infos = scene
        .problems
        .iter()
        .filter(|p| p.severity == "info")
        .count();
    (errors, warnings, infos)
}
