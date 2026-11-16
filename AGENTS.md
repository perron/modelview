# Working on modelview

This is a viewer and an inspector. The only write path is glTF export, and
the MCP server writes only when a tool is given an output path inside an
allow folder. Do not add editing, scripting, or a network client.

## Rules that are easy to break

- Load files in the child process (`load_isolated`) from the CLI, the window,
  and MCP. `load_in_process` is for the worker and for unit tests. A new
  parser must return `MvError`, not panic, including on truncated and random
  bytes. `tests` and `src/load/mod.rs` cover that.
- Do not copy GPL code. ufbx is MIT or Unlicense. PolyLoupe, MeshLab, and
  Blender are not available to copy from.
- Do not add the Autodesk FBX SDK.
- New sample files need a paragraph in `testdata/LICENSES.md`. No paid or
  EULA-bound assets in the tree.
- JSON objects keep their `schema` string. Adding a field is fine. Renaming
  or removing one needs a new schema version (`modelview/info/v2`) and a note
  in `docs/schemas/`.
- `Problem` is serialized with bincode between the worker and the parent.
  Do not put `skip_serializing_if` on fields of `Scene` or anything it
  contains. bincode will not round-trip that.
- Skinning stays on the CPU unless you also keep the CPU path for tests.
  The PNG the agent gets and the PNG the window shows have to match.

## Commands

```sh
cargo test
MODELVIEW_SOFTWARE=1 cargo test
MODELVIEW_SOFTWARE=1 MODELVIEW_UPDATE_GOLDEN=1 cargo test --test corpus render_matches
```

`MODELVIEW_WORKER_SLEEP_MS` makes the loader sleep before parsing. The
timeout test uses it. It is not a public flag.

Linux render tests need lavapipe (`mesa-vulkan-drivers`) and the X11/xcb
packages eframe links. The CI workflow lists them.

Exported GLBs are checked with Khronos `gltf_validator` 2.0.0-dev.3.10 when
`GLTF_VALIDATOR` is set or `gltf_validator` is on `PATH`. Linux CI sets
`MODELVIEW_REQUIRE_VALIDATOR=1`, so a missing binary fails the test there.

## Where to change things

| Area | File |
| --- | --- |
| FBX | `src/load/fbx.rs` |
| glTF | `src/load/gltf.rs` |
| OBJ, STL, PLY | `src/load/obj.rs`, `stl.rs`, `ply.rs` |
| Texture search | `src/load/textures.rs` |
| Scene, skinning, animation sampling | `src/scene.rs` |
| Problem codes | loaders and `src/validate.rs` |
| Pixels | `src/render.rs`, `src/shader.wgsl` |
| GLB | `src/export_glb.rs` |
| Arguments | `src/cli.rs` |
| Window | `src/gui.rs` |
| MCP | `src/mcp_server.rs` |

FBX is normalized to meters and Y-up with ufbx `target_axes` /
`target_unit_meters` and `SpaceConversion::AdjustTransforms`. Geometry
transforms are baked into the vertices (`ModifyGeometry`). The info JSON
still reports the file's original unit and up axis.
