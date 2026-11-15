#!/usr/bin/env python3
"""Render the MSIX tile assets from one source image.

    ./scripts/make-store-assets.py

MSIX wants fixed names at fixed sizes, plus optional `scale-200`/`scale-400`
variants for high-DPI displays, and `targetsize-*_altform-unplated` for the
Start-menu list. The source is the microphone mark used for the application
icons, centred on the package's background colour so a tile reads as one shape.

Re-run after changing the artwork; the output is committed because a package
build should not need Python.
"""
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "icons" / "microphone-icon-1024.png"
OUT = ROOT / "packaging" / "assets"
BACKGROUND = (32, 35, 41, 255)  # the panel colour the app uses
INSET = 0.62  # the mark occupies this share of the tile

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


def render(name: str, width: int, height: int, path: Path, mark: Image.Image) -> None:
    canvas = Image.new("RGBA", (width, height), BACKGROUND)
    box = int(min(width, height) * INSET)
    scaled = mark.resize((box, box), Image.LANCZOS)
    canvas.alpha_composite(scaled, ((width - box) // 2, (height - box) // 2))
    canvas.save(path, "PNG")
    print(f"{path.relative_to(ROOT)}  {width}x{height}")


def main() -> None:
    mark = Image.open(SOURCE).convert("RGBA")
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
