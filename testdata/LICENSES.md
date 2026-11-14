# Test corpus licenses

Every file under `testdata/` is here so CI can load it. Nothing in this
directory is a commercial game asset.

## `generated/`

Written by `tools/make_fixtures.py`. These files were made for this
repository. They are dedicated to the public domain under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/).

That includes the checker PNG, the OBJ/MTL pair, both STL files, both PLY
files, `embedded_cube.glb`, and the corrupt fixtures.

## `ufbx/`

Copied from the [ufbx](https://github.com/ufbx/ufbx) repository, directory
`data/`, at commit `1d097696e33b367bb3152b175aeb2d95193f9b6e` (2026-10-01).
The ufbx repository is dual-licensed under the MIT license or the Unlicense,
copyright Samuli Raivio. These particular files are the project's own test
exports (Blender primitives, a skinned "sausage" rig, a synthetic FBX 7.7
polygon, an embedded-texture material test). No separate third-party license
is declared for them in that repository. They are redistributed here under
the MIT option, with the ufbx copyright notice.

Files:

| File | Why it is here |
| --- | --- |
| `blender_279_default_6100_ascii.fbx` | FBX 6.1 ASCII |
| `blender_279_default_7400_binary.fbx` | FBX 7.4 binary of the same kind of scene |
| `blender_279_default.obj` | Reference mesh for the default cube |
| `blender_279_sausage_6100_ascii.fbx` | Skinned, animated FBX 6.1 ASCII |
| `blender_279_sausage_7400_binary.fbx` | Same rig, FBX 7.4 binary |
| `blender_279_sausage_base_0.obj` | Reference shape for the sausage bind pose |
| `blender_279_uv_sets_7400_binary.fbx` | More than one UV set |
| `blender_293_embedded_textures_7400_binary.fbx` | Embedded textures |
| `synthetic_direct_by_polygon_7700_ascii.fbx` | FBX 7.7 ASCII, authored as a synthetic fixture |
| `maya_absolute_texture_7700_ascii.fbx` | Absolute texture path |

## `khronos/BoxTextured/`

[BoxTextured](https://github.com/KhronosGroup/glTF-Sample-Assets/tree/main/Models/BoxTextured)
from Khronos glTF-Sample-Assets. The model files are
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/), copyright 2017 Cesium.
The texture contains the Cesium logo. Khronos's `LICENSE.md` for the model is
copied next to the file. Attribution: "Box Textured" by Cesium, via the
Khronos glTF Sample Assets.

## `golden/`

PNG renders produced by this repository's tests on lavapipe (Mesa's software
Vulkan). They are test output, not third-party art. Regenerate them with
`MODELVIEW_SOFTWARE=1 MODELVIEW_UPDATE_GOLDEN=1 cargo test --test corpus`.
