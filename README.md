# modelview

A desktop viewer for the 3D files a game project actually has: FBX (binary and
ASCII, 6.1 through 7.7), glTF 2.0 (`.gltf` and `.glb`), OBJ with MTL, STL, and
PLY. The same binary is the window, the command line, and a stdio MCP server,
so a person and an agent inspect a file with the same loader and the same
renderer.

Version 0.1. One Rust binary. macOS (Apple silicon and Intel, one universal
build) and Windows 11. Linux is supported for tests and headless rendering;
it is not a packaging target.

## What v0.1 does not do

- USD, USDZ, `.blend`, `.uasset`, Collada, and 3DS are not loaded. USD is the
  planned v0.2 input.
- Blend shapes are reported and not applied. Dual-quaternion skinning is
  drawn as linear blend skinning. Constraints and IK are not shown.
- At most four bone influences are kept. Extra influences are dropped,
  renormalized, and reported as `MAX_INFLUENCES_GT_4`.
- Skinning runs on the CPU. That is the same path the screenshots use, so a
  GPU difference cannot change the pose. It is slower than a vertex shader
  on a heavy character.
- Materials are a preview. ufbx's unified PBR values and glTF metallic-roughness
  are what the shaded view uses. It will not match Unreal or Unity.
- Normal maps, emission-only maps, and blend-shape sliders are not drawn.
- DDS is decoded when `image_dds` can read it. Other GPU-compressed files that
  it cannot read show up as `TEXTURE_DECODE_FAILED`.
- The macOS and Windows builds are unsigned. macOS will ask you to open the
  app from the Finder context menu the first time. Windows SmartScreen will
  warn. Signing costs money and needs the owner's approval.
- The window redraws the 3D view by reading the framebuffer back. Orbiting a
  large scene is interactive, not a 60 fps editor viewport.
- There is no file association. The app will not take `.fbx` away from Unreal
  or Unity.

## Install

Download the universal macOS zip or dmg, or the Windows zip, from the GitHub
Actions artifacts on a release build. They are unsigned.

macOS, after unzipping:

```sh
xattr -dr com.apple.quarantine Modelview.app
open Modelview.app
```

Or run the binary inside the app:

```sh
Modelview.app/Contents/MacOS/modelview
```

Windows: unzip and run `modelview.exe`. The optional NSIS installer copies it
to `%LOCALAPPDATA%\modelview\` and does not edit `PATH`.

From source (stable Rust):

```sh
cargo build --release
./target/release/modelview
```

On Linux the crate also needs `libxkbcommon`, `libxcb`, and a Vulkan driver.
Headless tests use lavapipe (`mesa-vulkan-drivers`).

## Window

`modelview` with no arguments opens an empty window. `modelview path/to/file.fbx`
opens that file. You can also use File → Open, or drop a file on the window.

- Left drag orbits. Right drag or middle drag pans. The scroll wheel zooms.
- Front, Back, Left, Right, Top, and 3/4 jump the camera. `F` returns to the
  three-quarter angle and does not reset pan or zoom.
- Shaded, Wire, Normals, UV, and Weights switch the view. The UV menu picks
  the set. Weights colors the mesh by the selected bone; click a bone in the
  outliner to change it.
- Skeleton draws bones on top of the mesh when the file has any.
- The outliner is the node tree. The inspector shows the selection plus
  scene, material, and texture stats. Problems lists the same issues
  `validate` prints, including every path a missing texture was looked up in.
- The take menu, Play, and the time slider scrub animation. Loading happens
  in a child process. A file that hangs is stopped after 60 seconds and the
  window stays up.

External textures are looked up as written, relative to the model, in the
model folder, and in `Textures/` or `textures/` next to the model and in the
parent folder. After those misses, the same folders are scanned without
regard to case. That scan is what Windows does for you anyway, and it is what
makes the same asset resolve on Linux.

## Command line

`modelview --help` lists the subcommands. Add `--json` for versioned JSON.
If stdout is not a terminal, JSON is the default. Human text is only for an
interactive terminal without `--json`.

`--timeout SECONDS` (default 30) stops a loader that has not finished.
`--memory-mb N` (default 2048) caps the loader's address space on Unix.
The loader is a child process. A crash or a timeout becomes a JSON error and
does not kill the caller.

Exit codes: 0 success, 1 warnings with `validate --strict`, 2 the file or the
command failed, 3 the file could not be read (`FILE_NOT_FOUND`, `TIMEOUT`,
`IO_ERROR`, `PERMISSION_DENIED`).

```sh
modelview info hero.fbx --json
modelview validate hero.fbx --json --strict
modelview render hero.fbx -o front.png --view front --size 1024x1024 --mode shaded
modelview render hero.fbx -o weights.png --mode weights --bone Hips --view three-quarter
modelview render hero.fbx -o frame.png --anim Walk --time 0.4 --yaw 30 --pitch 15
modelview turntable hero.fbx -o sheet.png --frames 8 --size 256x256
modelview export glb hero.fbx -o hero.glb
modelview list-animations hero.fbx --json
```

`--view` is `front`, `back`, `left`, `right`, `top`, or `three-quarter`.
`--yaw` and `--pitch` are degrees and override that preset. `--mode` is
`shaded`, `wireframe`, `normals`, `uv-checker`, or `weights`. `--size` is
`WIDTHxHEIGHT`.

`--detail` on `info` is `all` or a comma-separated list of `nodes`, `meshes`,
`materials`, `textures`, `anims`.

JSON schemas for the stable fields are in `docs/schemas/`. Every object has
`schema` (for example `modelview/info/v1`) and `ok`. Errors look like:

```json
{"schema": "modelview/error/v1", "ok": false, "code": "PARSE_FAILED", "message": "..."}
```

Codes you can branch on: `FILE_NOT_FOUND`, `PARSE_FAILED`, `TIMEOUT`,
`UNSUPPORTED_EXTENSION`, `TEXTURE_MISSING`, `TEXTURE_ABSOLUTE_PATH`, `TEXTURE_DECODE_FAILED`,
`DEGENERATE_GEOMETRY`, `UNSUPPORTED_FEATURE`, `PERMISSION_DENIED`,
`INVALID_ARGUMENT`, `IO_ERROR`, `RENDER_FAILED`, `RENDER_EMPTY`, `EXPORT_FAILED`.
`RENDER_EMPTY` means the picture is a solid colour, so `render` exits non-zero
instead of writing `ok: true`.
A binary STL is accepted when its length is exactly `84 + 50 × n` triangles,
including meshes with millions of triangles. A count that does not match
the length is rejected. Repeated corners are stored once.

Opening a model only reads it. modelview does not hard-link, clone, or write
extended attributes, so the source file's ctime, mtime, link count, and xattrs
stay put. The first APFS `clonefile` of a file updates that file's ctime, which
is why the macOS temp copy is a new file written from a read-only open. An
xattr such as `com.apple.lastuseddate` or `com.apple.provenance` is added by
macOS itself, not by this program.

`validate` also reports `UNITS_CM`, `UNITS_MM`, `UNITS_NOTE`, `UNUSUAL_SCALE`, `AXIS_NOTE`,
`ASCII_FBX`, `OLD_FBX_VERSION`, `MISSING_UV`, `MAX_INFLUENCES_GT_4`,
`NEGATIVE_SCALE`, `EMPTY_MESH`, and similar notes. Infos do not fail
`--strict`. Warnings do.

Headless rendering needs Vulkan, Metal, or Direct3D 12. Set
`MODELVIEW_SOFTWARE=1` to force a software adapter (lavapipe on Linux, WARP
on Windows). The render JSON includes the adapter name.

## MCP

```sh
modelview mcp --allow /path/to/assets --allow /path/to/more
```

Or a config file:

```json
{ "allow": ["/path/to/assets"] }
```

```sh
modelview mcp --config modelview-mcp.json
```

The server speaks MCP over stdin and stdout. It does not listen on a port.
Tools: `model_info`, `model_validate`, `model_render`, `model_turntable`,
`model_list_animations`, `model_find`. A path outside the allow list returns
`PERMISSION_DENIED`. `model_render` and `model_turntable` return a PNG in the
tool result. They write a file only when you pass `output`, and that path
must also be inside an allow folder. The server does not run code from a
model file.

Claude Code (`.mcp.json` in the project, or `claude mcp add`):

```json
{
  "mcpServers": {
    "modelview": {
      "command": "modelview",
      "args": ["mcp", "--allow", "/absolute/path/to/assets"]
    }
  }
}
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.modelview]
command = "modelview"
args = ["mcp", "--allow", "/absolute/path/to/assets"]
```

Cursor (`.cursor/mcp.json` in the project, or Settings → MCP):

```json
{
  "mcpServers": {
    "modelview": {
      "command": "modelview",
      "args": ["mcp", "--allow", "/absolute/path/to/assets"]
    }
  }
}
```

Use the absolute path of the binary if it is not on `PATH`.

## Build and test

```sh
cargo test
MODELVIEW_SOFTWARE=1 cargo test
```

`MODELVIEW_SOFTWARE=1` asks wgpu for a fallback adapter. On Linux that is
lavapipe. Golden PNGs in `testdata/golden/` were rendered that way. Regenerate
them with `MODELVIEW_SOFTWARE=1 MODELVIEW_UPDATE_GOLDEN=1 cargo test --test corpus`.
On a real GPU the same test allows a larger difference. CI on Linux installs
lavapipe and the Khronos `gltf_validator` 2.0.0-dev.3.10 binary and sets
`MODELVIEW_REQUIRE_VALIDATOR=1`. macOS and Windows CI render with whatever
adapter the runner has, upload the PNGs, and do not have to match lavapipe
pixel for pixel.

`tools/make_fixtures.py` rewrites `testdata/generated/`.

## Layout

The loader for each format lives under `src/load/`. `load_isolated` spawns
this binary as `--load-worker` and kills it on timeout. The window, the
subcommands, and the MCP tools all go through that. `src/render.rs` is the
one wgpu renderer. `src/export_glb.rs` writes GLB; ufbx does not.

FBX goes through the `ufbx` crate (ufbx C 0.23.1). glTF goes through the
`gltf` crate. OBJ, STL, and PLY are parsed in this repository. The window is
eframe/egui. The MCP server is the `rmcp` crate.
