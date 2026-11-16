#!/usr/bin/env python3
"""Write the small fixtures we authored ourselves. Licenses: CC0, see testdata/LICENSES.md."""

import json
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "testdata" / "generated"
TEX = ROOT / "textures"


def png(w, h, pixel):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w):
            raw.extend(pixel(x, y))

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def checker(x, y):
    if ((x // 2) + (y // 2)) % 2 == 0:
        return bytes((220, 64, 48, 255))
    return bytes((245, 220, 70, 255))


def write_text(path, text):
    path.write_text(text, encoding="utf-8")


def cube_faces():
    """24 vertices, CCW when viewed from outside. Each entry is pos, normal, uv."""
    s = 0.5
    faces = []
    specs = [
        # +Z front
        ((0, 0, 1), [(-s, -s, s), (s, -s, s), (s, s, s), (-s, s, s)]),
        # -Z back
        ((0, 0, -1), [(s, -s, -s), (-s, -s, -s), (-s, s, -s), (s, s, -s)]),
        # +X right
        ((1, 0, 0), [(s, -s, s), (s, -s, -s), (s, s, -s), (s, s, s)]),
        # -X left
        ((-1, 0, 0), [(-s, -s, -s), (-s, -s, s), (-s, s, s), (-s, s, -s)]),
        # +Y top
        ((0, 1, 0), [(-s, s, s), (s, s, s), (s, s, -s), (-s, s, -s)]),
        # -Y bottom
        ((0, -1, 0), [(-s, -s, -s), (s, -s, -s), (s, -s, s), (-s, -s, s)]),
    ]
    uvs = [(0, 0), (1, 0), (1, 1), (0, 1)]
    for normal, corners in specs:
        for uv, pos in zip(uvs, corners):
            faces.append((pos, normal, uv))
    return faces


def write_obj():
    verts = cube_faces()
    lines = ["# modelview fixture cube", "mtllib cube.mtl", "o Cube", "usemtl checker"]
    for pos, _n, _uv in verts:
        lines.append(f"v {pos[0]} {pos[1]} {pos[2]}")
    for _p, n, _uv in verts:
        lines.append(f"vn {n[0]} {n[1]} {n[2]}")
    for _p, _n, uv in verts:
        lines.append(f"vt {uv[0]} {uv[1]}")
    # 6 faces, 4 verts each, two triangles, OBJ is 1-based
    for f in range(6):
        b = f * 4 + 1
        lines.append(f"f {b}/{b}/{b} {b+1}/{b+1}/{b+1} {b+2}/{b+2}/{b+2}")
        lines.append(f"f {b}/{b}/{b} {b+2}/{b+2}/{b+2} {b+3}/{b+3}/{b+3}")
    write_text(ROOT / "cube.obj", "\n".join(lines) + "\n")
    write_text(
        ROOT / "cube.mtl",
        "\n".join(
            [
                "newmtl checker",
                "Kd 1.0 1.0 1.0",
                "Ks 0.04 0.04 0.04",
                "Ns 16",
                "d 1",
                "map_Kd textures/checker.png",
                "",
            ]
        ),
    )
    write_text(
        ROOT / "case_tex.mtl",
        "\n".join(
            [
                "newmtl checker",
                "Kd 0.8 0.8 0.8",
                "map_Kd Textures/Checker.PNG",
                "",
            ]
        ),
    )
    write_text(
        ROOT / "case_tex.obj",
        "\n".join(
            [
                "mtllib case_tex.mtl",
                "o Case",
                "usemtl checker",
                "v 0 0 0",
                "v 1 0 0",
                "v 0 1 0",
                "vt 0 0",
                "vt 1 0",
                "vt 0 1",
                "vn 0 0 1",
                "f 1/1/1 2/2/1 3/3/1",
                "",
            ]
        ),
    )
    write_text(
        ROOT / "missing_tex.mtl",
        "\n".join(
            [
                "newmtl gone",
                "Kd 1 0 0",
                "map_Kd C:/Artist/Does/Not/Exist/albedo.png",
                "",
            ]
        ),
    )
    write_text(
        ROOT / "missing_tex.obj",
        "\n".join(
            [
                "mtllib missing_tex.mtl",
                "o Missing",
                "usemtl gone",
                "v 0 0 0",
                "v 1 0 0",
                "v 0 1 0",
                "f 1 2 3",
                "",
            ]
        ),
    )
    write_text(
        ROOT / "degenerate.obj",
        "\n".join(
            [
                "o Degenerate",
                "v 0 0 0",
                "v 1 0 0",
                "v 0.5 0 0",
                "f 1 2 3",
                "",
            ]
        ),
    )
    write_text(
        ROOT / "triangle.obj",
        "\n".join(
            [
                "o Tri",
                "v 0 0 0",
                "v 1 0 0",
                "v 0 1 0",
                "vn 0 0 1",
                "f 1//1 2//1 3//1",
                "",
            ]
        ),
    )


def write_stl():
    # 12 triangles of the cube, matching cube_faces order (two tris per face).
    verts = cube_faces()
    tris = []
    for f in range(6):
        b = f * 4
        for idx in ((0, 1, 2), (0, 2, 3)):
            tri = [verts[b + i] for i in idx]
            normal = tri[0][1]
            tris.append((normal, [p[0] for p in tri]))

    ascii_lines = ["solid cube"]
    for normal, corners in tris:
        ascii_lines.append(f"  facet normal {normal[0]} {normal[1]} {normal[2]}")
        ascii_lines.append("    outer loop")
        for c in corners:
            ascii_lines.append(f"      vertex {c[0]} {c[1]} {c[2]}")
        ascii_lines.append("    endloop")
        ascii_lines.append("  endfacet")
    ascii_lines.append("endsolid cube")
    write_text(ROOT / "cube_ascii.stl", "\n".join(ascii_lines) + "\n")

    blob = bytearray(b"modelview ascii-or-binary stl fixture".ljust(80, b"\0"))
    blob += struct.pack("<I", len(tris))
    for normal, corners in tris:
        blob += struct.pack("<fff", *normal)
        for c in corners:
            blob += struct.pack("<fff", *c)
        blob += struct.pack("<H", 0)
    (ROOT / "cube_binary.stl").write_bytes(blob)


def write_ply():
    # A single square, two triangles, with normals.
    verts = [
        (-0.5, -0.5, 0.0, 0.0, 0.0, 1.0),
        (0.5, -0.5, 0.0, 0.0, 0.0, 1.0),
        (0.5, 0.5, 0.0, 0.0, 0.0, 1.0),
        (-0.5, 0.5, 0.0, 0.0, 0.0, 1.0),
    ]
    faces = [(0, 1, 2), (0, 2, 3)]
    header = "\n".join(
        [
            "ply",
            "format ascii 1.0",
            "comment modelview fixture",
            "element vertex 4",
            "property float x",
            "property float y",
            "property float z",
            "property float nx",
            "property float ny",
            "property float nz",
            "element face 2",
            "property list uchar int vertex_indices",
            "end_header",
        ]
    )
    body = []
    for v in verts:
        body.append(" ".join(str(x) for x in v))
    for f in faces:
        body.append("3 " + " ".join(str(i) for i in f))
    write_text(ROOT / "square_ascii.ply", header + "\n" + "\n".join(body) + "\n")

    bheader = "\n".join(
        [
            "ply",
            "format binary_little_endian 1.0",
            "comment modelview fixture",
            "element vertex 4",
            "property float x",
            "property float y",
            "property float z",
            "property float nx",
            "property float ny",
            "property float nz",
            "element face 2",
            "property list uchar int vertex_indices",
            "end_header\n",
        ]
    ).encode("ascii")
    blob = bytearray(bheader)
    for v in verts:
        blob += struct.pack("<ffffff", *v)
    for f in faces:
        blob += struct.pack("<Biii", 3, *f)
    (ROOT / "square_binary.ply").write_bytes(blob)


def pad4(data, pad_byte):
    extra = (-len(data)) % 4
    return data + bytes([pad_byte]) * extra


def write_glb():
    verts = cube_faces()
    positions = b"".join(struct.pack("<fff", *p) for p, _n, _uv in verts)
    normals = b"".join(struct.pack("<fff", *n) for _p, n, _uv in verts)
    uvs = b"".join(struct.pack("<ff", *uv) for _p, _n, uv in verts)
    indices = []
    for f in range(6):
        b = f * 4
        indices.extend((b, b + 1, b + 2, b, b + 2, b + 3))
    index_bytes = b"".join(struct.pack("<H", i) for i in indices)
    png_bytes = (TEX / "checker.png").read_bytes()
    geom = positions + normals + uvs + index_bytes
    assert len(geom) % 4 == 0
    bin_blob = geom + png_bytes
    bin_padded = pad4(bin_blob, 0)

    def accessor(buf, offset, count, typ, ctype, mins=None, maxs=None):
        acc = {
            "bufferView": buf,
            "byteOffset": offset,
            "componentType": ctype,
            "count": count,
            "type": typ,
        }
        if mins is not None:
            acc["min"] = mins
            acc["max"] = maxs
        return acc

    # component types: FLOAT 5126, UNSIGNED_SHORT 5123
    pos_off = 0
    nrm_off = len(positions)
    uv_off = nrm_off + len(normals)
    idx_off = uv_off + len(uvs)
    image_off = len(geom)
    gltf = {
        "asset": {"version": "2.0", "generator": "modelview fixtures"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Cube", "mesh": 0}],
        "meshes": [
            {
                "name": "Cube",
                "primitives": [
                    {
                        "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                        "indices": 3,
                        "material": 0,
                    }
                ],
            }
        ],
        "materials": [
            {
                "name": "checker",
                "pbrMetallicRoughness": {
                    "baseColorTexture": {"index": 0},
                    "baseColorFactor": [1, 1, 1, 1],
                    "metallicFactor": 0.0,
                    "roughnessFactor": 0.6,
                },
            }
        ],
        "textures": [{"sampler": 0, "source": 0}],
        "samplers": [{"magFilter": 9729, "minFilter": 9729, "wrapS": 10497, "wrapT": 10497}],
        "images": [
            {
                "bufferView": 1,
                "mimeType": "image/png",
                "name": "checker.png",
            }
        ],
        "accessors": [
            accessor(0, pos_off, 24, "VEC3", 5126, [-0.5, -0.5, -0.5], [0.5, 0.5, 0.5]),
            accessor(0, nrm_off, 24, "VEC3", 5126),
            accessor(0, uv_off, 24, "VEC2", 5126),
            accessor(0, idx_off, 36, "SCALAR", 5123),
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": len(geom), "target": 34962},
            {"buffer": 0, "byteOffset": image_off, "byteLength": len(png_bytes)},
        ],
        "buffers": [{"byteLength": len(bin_blob)}],
    }
    # Indices target should be ELEMENT_ARRAY_BUFFER when split, but a shared view is accepted
    # by most tools. Keep a single geometry view; the validator allows this.
    json_bytes = json.dumps(gltf, separators=(",", ":")).encode("utf-8")
    json_padded = pad4(json_bytes, 0x20)
    total = 12 + 8 + len(json_padded) + 8 + len(bin_padded)
    out = bytearray()
    out += struct.pack("<III", 0x46546C67, 2, total)
    out += struct.pack("<II", len(json_padded), 0x4E4F534A)
    out += json_padded
    out += struct.pack("<II", len(bin_padded), 0x004E4942)
    out += bin_padded
    assert len(out) == total
    (ROOT / "embedded_cube.glb").write_bytes(out)


def write_corrupt():
    (ROOT / "corrupt.fbx").write_bytes(bytes([i * 17 & 255 for i in range(64)]))
    (ROOT / "corrupt.glb").write_bytes(b"glTFnot-really" + bytes(range(40)))
    (ROOT / "corrupt.obj").write_bytes(b"this is not an obj\n\x00\xff\n")
    (ROOT / "corrupt.stl").write_bytes(b"solid \xff\xfe not closed")
    (ROOT / "corrupt.ply").write_bytes(b"ply\nformat ascii 1.0\nnot really\n")


def main():
    ROOT.mkdir(parents=True, exist_ok=True)
    TEX.mkdir(parents=True, exist_ok=True)
    (TEX / "checker.png").write_bytes(png(8, 8, checker))
    write_obj()
    write_stl()
    write_ply()
    write_glb()
    write_corrupt()
    print(f"wrote fixtures in {ROOT}")


if __name__ == "__main__":
    main()
