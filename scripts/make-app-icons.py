#!/usr/bin/env python3
"""Render every icon size the app ships from the source illustration.

`icons/app.png` is the 1024 px scene and stays as it is, for docs and a future
Store listing. It cannot be the executable icon: the wooden table fills the
bottom half edge to edge, so at 16 px the app turns into a brown bar.

Everything Windows and Linux actually load is cut from the microphone capsule
instead - grille, collar and red cross - which still reads at 16 px. The red
cross sits at x 360-550, y 296-492; the crop below keeps it with room to spare.

Usage:
    python3 scripts/make-app-icons.py
"""

import argparse
from pathlib import Path

from PIL import Image, ImageFilter

ROOT = Path(__file__).resolve().parent.parent

# Capsule box in the 1024 px source. Below y=600 is table, which is excluded.
CAPSULE = (250, 0, 780, 560)

# Windows asks for these at 100 %, 125 %, 150 %, 200 % DPI; 20 and 40 are the
# in-between frames that stop Explorer from resampling a neighbour.
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
# The notification area asks for the small icon metric: 16, 20, 24 or 32.
TRAY_SIZES = (16, 20, 24, 32)
# freedesktop names 22 because some panels ask for it; 512 is the software
# centre size. The capsule is 560 px square, so even 512 is a downscale.
HICOLOR_SIZES = (16, 22, 24, 32, 48, 64, 128, 256, 512)

# The settings header draws at 28 logical px; 128 covers a 4x display.
LOGO_SIZE = 128
# The taskbar and Alt-Tab bitmap. One RGBA frame, the compositor scales it.
WINDOW_ICON_SIZE = 256


def capsule(source: Image.Image) -> Image.Image:
    """Cut the microphone head out of the scene and centre it on a square."""
    cut = source.crop(CAPSULE)
    side = max(cut.size)
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.alpha_composite(cut, ((side - cut.width) // 2, (side - cut.height) // 2))
    return canvas


def render(mark: Image.Image, size: int) -> Image.Image:
    """Downsample once, from the crop, never from a smaller sibling."""
    return mark.resize((size, size), Image.LANCZOS)


def keyline(icon: Image.Image) -> Image.Image:
    """Give a small icon a one-pixel dark rim.

    The microphone is mostly light grey, and a taskbar can be light grey too.
    The artwork's own outlines are four pixels at 1024 and disappear below
    32 px, so the rim is what separates the mark from the panel.
    """
    alpha = icon.getchannel("A").point(lambda value: 255 if value > 24 else 0)
    alpha = alpha.filter(ImageFilter.MaxFilter(3))
    rim = Image.new("RGBA", icon.size, (24, 32, 40, 255))
    rim.putalpha(alpha)
    rim.alpha_composite(icon)
    return rim


def frame(mark: Image.Image, size: int) -> Image.Image:
    """A rendered frame, keylined at the sizes a panel actually draws."""
    icon = render(mark, size)
    return keyline(icon) if size <= 32 else icon


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
    parser.add_argument("--source", default="icons/microphone-icon-1024.png")
    parser.add_argument(
        "--hippo",
        default="../hippo-transcribe/icons/simple-hippo-square-icon.png",
        help="the original hippo, downscaled for the settings header",
    )
    args = parser.parse_args()

    source = Image.open(ROOT / args.source).convert("RGBA")
    mark = capsule(source)

    icons = ROOT / "icons"

    # The window bitmap, decoded at startup by the settings binary.
    render(mark, WINDOW_ICON_SIZE).save(icons / "app-256.png")
    print(f"{icons / 'app-256.png'}  {WINDOW_ICON_SIZE}")

    save_ico([frame(mark, size) for size in ICO_SIZES], ICO_SIZES, icons / "app.ico")
    save_ico([frame(mark, size) for size in TRAY_SIZES], TRAY_SIZES, icons / "tray.ico")

    # The header mark. It keeps its own rounded-square field, so it is not
    # cropped and not masked.
    hippo = Image.open(ROOT / args.hippo).convert("RGBA")
    logo = hippo.resize((LOGO_SIZE, LOGO_SIZE), Image.LANCZOS)
    logo.save(icons / "hippo.png")
    print(f"{icons / 'hippo.png'}  {LOGO_SIZE}")

    hicolor = ROOT / "packaging" / "hicolor"
    for size in HICOLOR_SIZES:
        target = hicolor / f"{size}x{size}" / "apps"
        target.mkdir(parents=True, exist_ok=True)
        frame(mark, size).save(target / "hippo-voice.png")
    print(f"{hicolor}  {', '.join(str(size) for size in HICOLOR_SIZES)}")


if __name__ == "__main__":
    main()
