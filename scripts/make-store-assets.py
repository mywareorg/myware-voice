#!/usr/bin/env python3
"""Render the MSIX tile assets from the monogram.

The source is the corrected MyWare monogram (icons/myware-voice-icons.svg):
a heavy square-shouldered M on a full-width blue baseline that ends in a
green terminal, with equal optical margins on all four sides. Every tile
puts the mark on the same light rounded-square field (22 % corner radius)
the app icon ships, so the Start-menu list, the taskbar bitmap and the
plated tiles are one object at every size.

rsvg-convert does the rasterization; regenerating needs it installed even
though the committed outputs do not. Small icons get a one-pixel dark
keyline so the light field does not vanish on a light surface.

Re-run after changing the artwork; the output is committed because a package
build should not need Python.

    ./scripts/make-store-assets.py
"""
import shutil
import subprocess
import tempfile
from pathlib import Path

from PIL import Image, ImageColor, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "icons" / "myware-voice-icons.svg"
OUT = ROOT / "packaging" / "assets"

# The tile the product ships first: light field, matching the light palette.
FIELD = ImageColor.getrgb("#EFF0F3") + (255,)
# 226 px at 1024, matching the myware-ui 16 px radius at 72 px.
RADIUS_RATIO = 226 / 1024
MASTER_SIZE = 1024
SUPERSAMPLE = 4
# At or below this size a one-pixel dark rim keeps a light tile visible on a
# light panel; both repositories draw the same keyline.
KEYLINE_LIMIT = 32
KEYLINE = (24, 32, 40, 255)
TRANSPARENT = (0, 0, 0, 0)

# name -> (width, height)
TILES = {
    "StoreLogo": (50, 50),
    "Square44x44Logo": (44, 44),
    "Square150x150Logo": (150, 150),
    "Wide310x150Logo": (310, 150),
    "SplashScreen": (620, 300),
}
# Start-menu list icons, named by their target size.
TARGETS = {"Square44x44Logo": [16, 24, 32, 48, 256]}
# Anything with a scale variant gets 200% and, for the small squares, 400%.
SCALES = {"Square44x44Logo": [2, 4], "Square150x150Logo": [2, 4], "Wide310x150Logo": [2], "SplashScreen": [2]}

# Tiles that fill their rectangle take the field colour rather than showing
# a transparent border; the square assets stay transparent so Windows and
# the Start menu supply their own backdrop.
FILLED = {"Wide310x150Logo", "SplashScreen"}


def render_svg(path: Path, size: int) -> Image.Image:
    """Rasterize the monogram with rsvg-convert at exactly `size`."""
    rsvg = shutil.which("rsvg-convert")
    if rsvg is None:
        raise SystemExit(
            f"{path.name} is SVG; install rsvg-convert (librsvg) to render it"
        )
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
        tmp_path = Path(tmp.name)
    try:
        subprocess.run(
            [rsvg, "-w", str(size), "-h", str(size), str(path), "-o", str(tmp_path)],
            check=True,
        )
        return Image.open(tmp_path).convert("RGBA")
    finally:
        tmp_path.unlink(missing_ok=True)


def rounded_mask(size: int) -> Image.Image:
    """Supersampled rounded-square alpha mask, radius 22 % of the side."""
    big = size * SUPERSAMPLE
    mask = Image.new("L", (big, big), 0)
    radius = round(big * RADIUS_RATIO)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, big - 1, big - 1], radius=radius, fill=255
    )
    return mask.resize((size, size), Image.LANCZOS)


def tile(mark: Image.Image, size: int) -> Image.Image:
    """The monogram on its light rounded-square field, side `size`.

    The monogram's own margins are the tile's padding, so the mark scales
    with the field rather than inside it.
    """
    field = Image.new("RGBA", (size, size), FIELD)
    field.alpha_composite(mark.resize((size, size), Image.LANCZOS))
    return Image.composite(
        field, Image.new("RGBA", (size, size)), rounded_mask(size)
    )


def keyline(icon: Image.Image) -> Image.Image:
    """Give a small icon a one-pixel dark rim.

    The field is #EFF0F3 and a light surface is the same grey, so the rim
    is what separates the tile from whatever it lands on.
    """
    alpha = icon.getchannel("A").point(lambda value: 255 if value > 24 else 0)
    alpha = alpha.filter(ImageFilter.MaxFilter(3))
    rim = Image.new("RGBA", icon.size, KEYLINE)
    rim.putalpha(alpha)
    rim.alpha_composite(icon)
    return rim


def render(name: str, width: int, height: int, path: Path, mark: Image.Image) -> None:
    """Write the width x height asset, rendered from the master.

    A square asset is exactly the tile at that side, the same frame
    make-app-icons.py writes, so the Start-menu list and the taskbar bitmap
    are the same object. The wide tile and the splash screen centre the
    tile on the field colour.
    """
    icon = tile(mark, min(width, height))
    if icon.size == (width, height):
        canvas = icon
    else:
        canvas = Image.new("RGBA", (width, height), FIELD if name in FILLED else TRANSPARENT)
        canvas.alpha_composite(
            icon, ((width - icon.width) // 2, (height - icon.height) // 2)
        )
    if min(width, height) <= KEYLINE_LIMIT:
        canvas = keyline(canvas)
    canvas.save(path, "PNG")
    print(f"{path.relative_to(ROOT)}  {width}x{height}")


def main() -> None:
    mark = render_svg(SOURCE, MASTER_SIZE)
    OUT.mkdir(parents=True, exist_ok=True)
    for name, (width, height) in TILES.items():
        render(name, width, height, OUT / f"{name}.png", mark)
        for scale in SCALES.get(name, []):
            render(name, width * scale, height * scale, OUT / f"{name}.scale-{scale * 100}.png", mark)
    for name, sizes in TARGETS.items():
        for size in sizes:
            render(name, size, size, OUT / f"{name}.targetsize-{size}_altform-unplated.png", mark)


if __name__ == "__main__":
    main()
