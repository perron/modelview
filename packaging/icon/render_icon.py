#!/usr/bin/env python3
"""Draw the modelview icon and write the PNG, ICNS, and ICO files.

The mark is an isometric cube on a dark rounded square. It is original
geometry, drawn here, with no text and no external artwork. Run this
script to regenerate the committed images.
"""

import struct
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
PNG_DIR = Path(__file__).resolve().parent
SIZES = (16, 32, 48, 64, 128, 256, 512, 1024)

# icns OSTypes. The @2x entries reuse the matching pixel size.
ICNS_TYPES = {
    16: (b"icp4",),
    32: (b"icp5", b"ic11"),
    64: (b"icp6", b"ic12"),
    128: (b"ic07",),
    256: (b"ic08", b"ic13"),
    512: (b"ic09", b"ic14"),
    1024: (b"ic10",),
}


def draw_icon(size: int) -> Image.Image:
    scale = 4
    canvas = size * scale
    image = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    radius = int(canvas * 0.22)
    draw.rounded_rectangle(
        (0, 0, canvas - 1, canvas - 1),
        radius=radius,
        fill=(26, 28, 34, 255),
    )

    cx = canvas * 0.50
    cy = canvas * 0.54
    w = canvas * 0.30
    h = w * 0.56
    top = (cx, cy - 2 * h)
    top_left = (cx - w, cy - h)
    top_right = (cx + w, cy - h)
    mid = (cx, cy)
    bottom_left = (cx - w, cy + h)
    bottom_right = (cx + w, cy + h)
    bottom = (cx, cy + 2 * h)

    draw.polygon((top_left, mid, bottom, bottom_left), fill=(78, 86, 98, 255))
    draw.polygon((top_right, bottom_right, bottom, mid), fill=(126, 134, 146, 255))
    draw.polygon((top, top_right, mid, top_left), fill=(214, 220, 228, 255))

    # A thin darker seam keeps the three faces distinct at 16 px.
    seam = max(2, canvas // 96)
    draw.line((top_left, mid, top_right), fill=(32, 36, 42, 255), width=seam)
    draw.line((mid, bottom), fill=(32, 36, 42, 255), width=seam)

    return image.resize((size, size), Image.Resampling.LANCZOS)


def write_pngs() -> dict[int, Image.Image]:
    icons = {}
    for size in SIZES:
        icon = draw_icon(size)
        icon.save(PNG_DIR / f"icon-{size}.png", optimize=True)
        icons[size] = icon
    return icons


def write_icns(icons: dict[int, Image.Image], path: Path) -> None:
    chunks = []
    for size, types in ICNS_TYPES.items():
        blob = png_bytes(icons[size])
        for ostype in types:
            chunks.append(ostype + struct.pack(">I", 8 + len(blob)) + blob)
    body = b"".join(chunks)
    path.write_bytes(b"icns" + struct.pack(">I", 8 + len(body)) + body)


def png_bytes(image: Image.Image) -> bytes:
    from io import BytesIO

    buf = BytesIO()
    image.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def write_ico(icons: dict[int, Image.Image], path: Path) -> None:
    ordered = [icons[size] for size in (16, 32, 48, 64, 128, 256)]
    ordered[-1].save(
        path,
        format="ICO",
        sizes=[(image.width, image.height) for image in ordered],
        append_images=ordered[:-1],
    )


def main() -> None:
    icons = write_pngs()
    write_icns(icons, ROOT / "macos" / "AppIcon.icns")
    write_ico(icons, ROOT / "windows" / "modelview.ico")


if __name__ == "__main__":
    main()
