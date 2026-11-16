#!/usr/bin/env python3
"""Render every icon size the app ships from the source artwork.

The source is the corrected MyWare monogram (icons/myware-voice-icons.svg):
a heavy square-shouldered M on a full-width blue baseline that ends in a
green terminal, with equal optical margins on all four sides. Windows and
Linux load the light rounded-square tile (22 % corner radius); only the
settings header draws the mark bare, without the tile field.

rsvg-convert does the rasterization; the hicolor ladder reaches 512 px, so
regenerating needs it installed even though the committed outputs do not.

Usage:
    python3 scripts/make-app-icons.py
"""

import argparse
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent

# Windows asks for these at 100 %, 125 %, 150 %, 200 % DPI; 20 and 40 are the
# in-between frames that stop Explorer from resampling a neighbour.
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
# The notification area asks for the small icon metric: 16, 20, 24 or 32.
TRAY_SIZES = (16, 20, 24, 32)
# freedesktop names 22 because some panels ask for it; 512 is the software
# centre size.
HICOLOR_SIZES = (16, 22, 24, 32, 48, 64, 128, 256, 512)

# The settings header draws the mark bare at 28 logical px; 128 covers a 4x
# display.
LOGO_SIZE = 128
# The taskbar and Alt-Tab bitmap. One RGBA frame, the compositor scales it.
WINDOW_ICON_SIZE = 256

# The tile the product ships first: light field, matching the light palette.
FIELD = (239, 240, 243, 255)  # EFF0F3
# 226 px at 1024, matching the myware-ui 16 px radius at 72 px.
RADIUS_RATIO = 226 / 1024
MASTER_SIZE = 1024
SUPERSAMPLE = 4
# At or below this size a one-pixel dark rim keeps the light tile separated
# from a light panel; both repositories draw the same keyline.
KEYLINE_LIMIT = 32
KEYLINE = (24, 32, 40, 255)


def render_svg(path: Path, size: int) -> Image.Image:
    """Rasterize the monogram with rsvg-convert at exactly `size`."""
    import shutil
    import subprocess
    import tempfile

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


def render(mark: Image.Image, size: int) -> Image.Image:
    """Downsample once, from the master, never from a smaller sibling."""
    return tile(mark, size)


def keyline(icon: Image.Image) -> Image.Image:
    """Give a small icon a one-pixel dark rim.

    The field is #EFF0F3 and a taskbar or panel can be light grey too, so the
    rim is what separates the tile from the surface it sits on.
    """
    alpha = icon.getchannel("A").point(lambda value: 255 if value > 24 else 0)
    alpha = alpha.filter(ImageFilter.MaxFilter(3))
    rim = Image.new("RGBA", icon.size, KEYLINE)
    rim.putalpha(alpha)
    rim.alpha_composite(icon)
    return rim


def frame(mark: Image.Image, size: int) -> Image.Image:
    """A rendered frame, keylined at the sizes a panel actually draws."""
    icon = render(mark, size)
    return keyline(icon) if size <= KEYLINE_LIMIT else icon


def save_ico(frames: list[Image.Image], sizes, path: Path) -> None:
    """Write a multi-frame .ico. Pillow stores each supplied frame verbatim."""
    frames[-1].save(
        path,
        format="ICO",
        sizes=[(size, size) for size in sizes],
        append_images=frames[:-1],
    )
    print(f"{path}  {', '.join(str(size) for size in sizes)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", default="icons/myware-voice-icons.svg")
    args = parser.parse_args()

    mark = render_svg(ROOT / args.source, MASTER_SIZE)

    icons = ROOT / "icons"

    # The window bitmap, decoded at startup by the settings binary.
    render(mark, WINDOW_ICON_SIZE).save(icons / "app-256.png")
    print(f"{icons / 'app-256.png'}  {WINDOW_ICON_SIZE}")

    save_ico([frame(mark, size) for size in ICO_SIZES], ICO_SIZES, icons / "app.ico")
    save_ico([frame(mark, size) for size in TRAY_SIZES], TRAY_SIZES, icons / "tray.ico")

    # The header mark. It is drawn on the settings window's own surface, so it
    # keeps no tile field: just the monogram on transparency.
    mark.resize((LOGO_SIZE, LOGO_SIZE), Image.LANCZOS).save(icons / "myware.png")
    print(f"{icons / 'myware.png'}  {LOGO_SIZE}")

    hicolor = ROOT / "packaging" / "hicolor"
    for size in HICOLOR_SIZES:
        target = hicolor / f"{size}x{size}" / "apps"
        target.mkdir(parents=True, exist_ok=True)
        frame(mark, size).save(target / "myware-voice.png")
    print(f"{hicolor}  {', '.join(str(size) for size in HICOLOR_SIZES)}")


if __name__ == "__main__":
    main()
