//! In-memory scene shared by the viewer, the CLI, and glTF export.
//!
//! Coordinates are Y-up, right-handed, in meters. Skinning is linear blend
//! on the CPU: each posed vertex is `sum weight * bone_world * inverse_bind * rest`.

use glam::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

pub const MAX_TRIANGLES: usize = 5_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scene {
    pub source: SourceInfo,
    pub nodes: Vec<Node>,
    pub meshes: Vec<Mesh>,
    pub materials: Vec<Material>,
    pub textures: Vec<Texture>,
    pub skins: Vec<Skin>,
    pub animations: Vec<Animation>,
    pub problems: Vec<Problem>,
    pub bounds: Aabb,
}

impl Scene {
    pub fn new(format: &str) -> Self {
        Self {
            source: SourceInfo {
                path: String::new(),
                file_name: String::new(),
                format: format.to_string(),
                size_bytes: 0,
                sha256: String::new(),
                fbx_version: None,
                fbx_ascii: None,
                creator: None,
                original_unit_meters: None,
                original_up_axis: None,
            },
            nodes: Vec::new(),
            meshes: Vec::new(),
            materials: Vec::new(),
            textures: Vec::new(),
            skins: Vec::new(),
            animations: Vec::new(),
            problems: Vec::new(),
            bounds: Aabb::empty(),
        }
    }

    pub fn add_problem(&mut self, problem: Problem) {
        if self.problems.iter().any(|p| p == &problem) {
            return;
        }
        self.problems.push(problem);
    }

    pub fn anim_index(&self, name: &str) -> Option<usize> {
        self.animations
            .iter()
            .position(|a| a.name.eq_ignore_ascii_case(name))
    }

    pub fn bone_index(&self, name: &str) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.is_bone && n.name.eq_ignore_ascii_case(name))
    }

    pub fn node_trs(&self, index: usize, anim: Option<usize>, time: f32) -> Trs {
        if let Some(ai) = anim {
            if let Some(animation) = self.animations.get(ai) {
                if let Some(track) = animation.tracks.iter().find(|t| t.node == index) {
                    return track.sample(time);
                }
            }
        }
        self.nodes[index].rest
    }

    pub fn world_matrices(&self, anim: Option<usize>, time: f32) -> Vec<Mat4> {
        let n = self.nodes.len();
        let mut worlds = vec![Mat4::IDENTITY; n];
        let mut done = vec![false; n];
        for i in 0..n {
            self.fill_world(i, anim, time, &mut worlds, &mut done, 0);
        }
        worlds
    }

    fn fill_world(
        &self,
        index: usize,
        anim: Option<usize>,
        time: f32,
        worlds: &mut [Mat4],
        done: &mut [bool],
        depth: u32,
    ) {
        if index >= self.nodes.len() || done[index] || depth > 512 {
            return;
        }
        done[index] = true;
        let local = self.node_trs(index, anim, time).matrix();
        if let Some(parent) = self.nodes[index].parent {
            if parent != index {
                self.fill_world(parent, anim, time, worlds, done, depth + 1);
                if parent < worlds.len() {
                    worlds[index] = worlds[parent] * local;
                    return;
                }
            }
        }
        worlds[index] = local;
    }

    /// Posed positions and normals. Skinned meshes are returned in world space.
    pub fn posed_mesh(&self, mesh_index: usize, worlds: &[Mat4]) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
        let mesh = &self.meshes[mesh_index];
        if mesh.joints.is_empty() || mesh.skin.is_none() {
            return (mesh.positions.clone(), mesh.normals.clone());
        }
        let skin = &self.skins[mesh.skin.unwrap()];
        let joint_mats: Vec<Mat4> = skin
            .joints
            .iter()
            .enumerate()
            .map(|(ji, &node)| {
                let ibm = skin
                    .inverse_bind
                    .get(ji)
                    .map(|m| Mat4::from_cols_array(m))
                    .unwrap_or(Mat4::IDENTITY);
                let bone = worlds.get(node).copied().unwrap_or(Mat4::IDENTITY);
                bone * ibm
            })
            .collect();

        let mut positions = Vec::with_capacity(mesh.positions.len());
        let mut normals = Vec::with_capacity(mesh.positions.len());
        for i in 0..mesh.positions.len() {
            let rest = Vec3::from(mesh.positions[i]);
            let rest_n = mesh
                .normals
                .get(i)
                .copied()
                .map(Vec3::from)
                .unwrap_or(Vec3::Y);
            let joints = mesh.joints.get(i).copied().unwrap_or([0, 0, 0, 0]);
            let weights = mesh.weights.get(i).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
            let mut pos = Vec3::ZERO;
            let mut nrm = Vec3::ZERO;
            let mut weight_sum = 0.0;
            for k in 0..4 {
                let w = weights[k];
                if w <= 0.0 {
                    continue;
                }
                let Some(mat) = joint_mats.get(joints[k] as usize).copied() else {
                    continue;
                };
                pos += w * mat.transform_point3(rest);
                let normal_mat = mat.inverse().transpose();
                nrm += w * normal_mat.transform_vector3(rest_n);
                weight_sum += w;
            }
            if weight_sum <= 0.0 {
                pos = rest;
                nrm = rest_n;
            }
            positions.push(pos.to_array());
            let n = nrm.normalize_or_zero();
            normals.push(if n.length_squared() > 0.0 {
                n.to_array()
            } else {
                rest_n.normalize_or_zero().to_array()
            });
        }
        (positions, normals)
    }

    pub fn recompute_bounds(&mut self) {
        let worlds = self.world_matrices(None, 0.0);
        let mut aabb = Aabb::empty();
        let mut drawn_skinned = vec![false; self.meshes.len()];
        for (ni, node) in self.nodes.iter().enumerate() {
            for &mi in &node.meshes {
                let mesh = &self.meshes[mi];
                let skinned = mesh.skin.is_some() && !mesh.joints.is_empty();
                if skinned {
                    if drawn_skinned[mi] {
                        continue;
                    }
                    drawn_skinned[mi] = true;
                    let (positions, _) = self.posed_mesh(mi, &worlds);
                    for p in positions {
                        aabb.include(p);
                    }
                } else {
                    let model = worlds.get(ni).copied().unwrap_or(Mat4::IDENTITY);
                    for p in &mesh.positions {
                        aabb.include(model.transform_point3(Vec3::from(*p)).to_array());
                    }
                }
            }
        }
        if !aabb.is_valid() {
            aabb = Aabb {
                min: [-0.5, -0.5, -0.5],
                max: [0.5, 0.5, 0.5],
            };
        }
        self.bounds = aabb;
    }

    pub fn triangle_count(&self) -> usize {
        self.meshes.iter().map(|m| m.indices.len() / 3).sum()
    }

    pub fn vertex_count(&self) -> usize {
        self.meshes.iter().map(|m| m.positions.len()).sum()
    }

    pub fn bone_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_bone).count()
    }

    /// Resolve which animation and time a render request asked for.
    /// Missing names produce a problem-style error.
    pub fn resolve_anim(
        &self,
        name: Option<&str>,
        time: Option<f32>,
    ) -> crate::Result<(Option<usize>, f32)> {
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            let time = time.unwrap_or(0.0);
            return Ok((None, time));
        };
        let Some(index) = self.anim_index(name) else {
            return Err(crate::MvError::invalid(format!(
                "animation '{name}' not found"
            )));
        };
        let duration = self.animations[index].duration;
        let time = time.unwrap_or(0.0).clamp(0.0, duration.max(0.0));
        Ok((Some(index), time))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceInfo {
    pub path: String,
    pub file_name: String,
    pub format: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub fbx_version: Option<u32>,
    pub fbx_ascii: Option<bool>,
    pub creator: Option<String>,
    pub original_unit_meters: Option<f32>,
    pub original_up_axis: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub rest: Trs,
    pub meshes: Vec<usize>,
    pub is_bone: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Trs {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl Trs {
    pub fn identity() -> Self {
        Self {
            translation: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
        }
    }

    pub fn matrix(self) -> Mat4 {
        let q = Quat::from_xyzw(
            self.rotation[0],
            self.rotation[1],
            self.rotation[2],
            self.rotation[3],
        );
        let q = if q.length_squared() > 1e-8 {
            q.normalize()
        } else {
            Quat::IDENTITY
        };
        Mat4::from_scale_rotation_translation(
            Vec3::from(self.scale),
            q,
            Vec3::from(self.translation),
        )
    }

    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let a = Quat::from_xyzw(
            self.rotation[0],
            self.rotation[1],
            self.rotation[2],
            self.rotation[3],
        );
        let b = Quat::from_xyzw(
            other.rotation[0],
            other.rotation[1],
            other.rotation[2],
            other.rotation[3],
        );
        let q = a.slerp(b, t);
        Self {
            translation: [
                lerp(self.translation[0], other.translation[0], t),
                lerp(self.translation[1], other.translation[1], t),
                lerp(self.translation[2], other.translation[2], t),
            ],
            rotation: [q.x, q.y, q.z, q.w],
            scale: [
                lerp(self.scale[0], other.scale[0], t),
                lerp(self.scale[1], other.scale[1], t),
                lerp(self.scale[2], other.scale[2], t),
            ],
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mesh {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<UvSet>,
    pub indices: Vec<u32>,
    /// Empty when the mesh is not skinned. Otherwise one entry per position.
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    pub material: Option<usize>,
    pub skin: Option<usize>,
}

impl Mesh {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            positions: Vec::new(),
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: Vec::new(),
            joints: Vec::new(),
            weights: Vec::new(),
            material: None,
            skin: None,
        }
    }

    pub fn max_influences(&self) -> usize {
        self.weights
            .iter()
            .map(|w| w.iter().filter(|c| **c > 1e-5).count())
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UvSet {
    pub name: String,
    pub coords: Vec<[f32; 2]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Material {
    pub name: String,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub base_color_texture: Option<usize>,
    pub shading_model: String,
    pub double_sided: bool,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            name: "default".into(),
            base_color: [0.75, 0.75, 0.78, 1.0],
            metallic: 0.0,
            roughness: 0.55,
            base_color_texture: None,
            shading_model: "default".into(),
            double_sided: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Texture {
    pub name: String,
    pub referenced_path: String,
    pub resolved_path: Option<String>,
    pub embedded: bool,
    /// `ok`, `missing`, or `decode_failed`. Embedded textures that decoded are `ok`.
    pub status: String,
    pub searched: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skin {
    pub name: String,
    pub joints: Vec<usize>,
    /// Column-major 4x4 inverse bind matrices, one per joint.
    pub inverse_bind: Vec<[f32; 16]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Animation {
    pub name: String,
    pub duration: f32,
    pub fps: f32,
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub node: usize,
    pub times: Vec<f32>,
    pub translation: Vec<[f32; 3]>,
    pub rotation: Vec<[f32; 4]>,
    pub scale: Vec<[f32; 3]>,
}

impl Track {
    pub fn sample(&self, time: f32) -> Trs {
        let n = self.times.len();
        if n == 0 {
            return Trs::identity();
        }
        if time <= self.times[0] || n == 1 {
            return self.at(0);
        }
        if time >= self.times[n - 1] {
            return self.at(n - 1);
        }
        let i = self.times.partition_point(|t| *t < time).saturating_sub(1);
        let t0 = self.times[i];
        let t1 = self.times[i + 1];
        let u = if t1 > t0 {
            (time - t0) / (t1 - t0)
        } else {
            0.0
        };
        self.at(i).lerp(self.at(i + 1), u)
    }

    fn at(&self, i: usize) -> Trs {
        Trs {
            translation: self.translation.get(i).copied().unwrap_or([0.0; 3]),
            rotation: self
                .rotation
                .get(i)
                .copied()
                .unwrap_or([0.0, 0.0, 0.0, 1.0]),
            scale: self.scale.get(i).copied().unwrap_or([1.0, 1.0, 1.0]),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Problem {
    pub code: String,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub searched: Vec<String>,
}

impl Problem {
    pub fn new(severity: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            severity: severity.to_string(),
            message: message.into(),
            path: None,
            searched: Vec::new(),
        }
    }

    pub fn info(code: &str, message: impl Into<String>) -> Self {
        Self::new("info", code, message)
    }

    pub fn warning(code: &str, message: impl Into<String>) -> Self {
        Self::new("warning", code, message)
    }

    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::new("error", code, message)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb {
    pub fn empty() -> Self {
        Self {
            min: [f32::MAX, f32::MAX, f32::MAX],
            max: [f32::MIN, f32::MIN, f32::MIN],
        }
    }

    pub fn include(&mut self, p: [f32; 3]) {
        if !p.iter().all(|c| c.is_finite()) {
            return;
        }
        for k in 0..3 {
            self.min[k] = self.min[k].min(p[k]);
            self.max[k] = self.max[k].max(p[k]);
        }
    }

    pub fn is_valid(self) -> bool {
        self.min[0] <= self.max[0] && self.min.iter().all(|c| c.is_finite())
    }

    pub fn center(self) -> [f32; 3] {
        if !self.is_valid() {
            return [0.0, 0.0, 0.0];
        }
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    pub fn radius(self) -> f32 {
        if !self.is_valid() {
            return 1.0;
        }
        let ext = Vec3::from(self.max) - Vec3::from(self.min);
        (ext.length() * 0.5).max(1e-4)
    }
}

pub fn push_checked(tris: &mut usize) -> crate::Result<()> {
    *tris += 1;
    if *tris > MAX_TRIANGLES {
        return Err(crate::MvError::parse(format!(
            "mesh has more than {MAX_TRIANGLES} triangles"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skinning_rotates_a_vertex_about_y() {
        let mut scene = Scene::new("test");
        scene.nodes.push(Node {
            name: "root".into(),
            parent: None,
            children: vec![1],
            rest: Trs::identity(),
            meshes: vec![],
            is_bone: false,
        });
        let mut bone = Trs::identity();
        // 90 degrees about +Y. A point on +X goes to -Z.
        let q = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        bone.rotation = [q.x, q.y, q.z, q.w];
        scene.nodes.push(Node {
            name: "bone".into(),
            parent: Some(0),
            children: vec![],
            rest: bone,
            meshes: vec![0],
            is_bone: true,
        });
        let mut mesh = Mesh::new("arm");
        mesh.positions = vec![[1.0, 0.0, 0.0]];
        mesh.normals = vec![[0.0, 1.0, 0.0]];
        mesh.indices = vec![];
        mesh.joints = vec![[0, 0, 0, 0]];
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]];
        mesh.skin = Some(0);
        scene.meshes.push(mesh);
        scene.skins.push(Skin {
            name: "skin".into(),
            joints: vec![1],
            inverse_bind: vec![Mat4::IDENTITY.to_cols_array()],
        });
        let worlds = scene.world_matrices(None, 0.0);
        let (positions, _) = scene.posed_mesh(0, &worlds);
        let p = positions[0];
        assert!(
            (p[0]).abs() < 1e-4 && p[1].abs() < 1e-4 && (p[2] + 1.0).abs() < 1e-4,
            "expected (0,0,-1), got {p:?}"
        );
    }

    #[test]
    fn track_sample_lerps_translation() {
        let track = Track {
            node: 0,
            times: vec![0.0, 1.0],
            translation: vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0]],
            rotation: vec![[0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0]],
            scale: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mid = track.sample(0.5);
        assert!((mid.translation[0] - 1.0).abs() < 1e-5);
    }
}
