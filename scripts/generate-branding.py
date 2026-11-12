"""Build OpenHinoVoice's vector-derived icons with Python, Pillow and headless Chrome.

Run from any directory: python scripts/generate-branding.py [--concepts-only]
The Chrome process is hidden; no installed profile is used. Nothing is downloaded.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import xml.etree.ElementTree as ET

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets" / "branding"
RENDER = ROOT / "target" / "branding-render"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
TRAY_SIZES = (16, 20, 24, 32, 40, 48, 64)
RED = "#BC002D"


def svg(title: str, body: str) -> str:
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256" role="img" aria-labelledby="title"><title id="title">{title}</title>{body}</svg>\n'


def inner_svg(text: str) -> str:
    return text[text.index(">") + 1:text.rindex("</svg>")]


def write_svg(path: Path, title: str, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(svg(title, body), encoding="utf-8", newline="\n")


def chrome_path() -> Path:
    paths = [
        Path(os.environ.get("PROGRAMFILES", r"C:\Program Files")) / "Google/Chrome/Application/chrome.exe",
        Path(os.environ.get("PROGRAMFILES(X86)", r"C:\Program Files (x86)")) / "Microsoft/Edge/Application/msedge.exe",
    ]
    for name in ("google-chrome", "chromium", "chrome", "msedge"):
        found = shutil.which(name)
        if found:
            paths.append(Path(found))
    for path in paths:
        if path.is_file():
            return path
    raise RuntimeError("Chrome or Edge is required to render the SVG sources")


def render_at_sizes(jobs: list[tuple[Path, int, Path]]) -> None:
    RENDER.mkdir(parents=True, exist_ok=True)
    width = 1280
    x = y = 8
    row_height = 0
    placements = []
    elements = []
    for source, size, output in jobs:
        if x + size + 8 > width:
            x = 8
            y += row_height + 16
            row_height = 0
        body = inner_svg(source.read_text(encoding="utf-8"))
        elements.append(f'<svg x="{x}" y="{y}" width="{size}" height="{size}" viewBox="0 0 256 256">{body}</svg>')
        placements.append((x, y, size, output))
        row_height = max(row_height, size)
        x += size + 16
    height = max(128, y + row_height + 8)
    page = RENDER / "sheet.html"
    shot = RENDER / "sheet.png"
    page.write_text(f'<!doctype html><html><meta charset="utf-8"><style>*{{margin:0;padding:0}}html,body{{background:transparent;overflow:hidden}}</style><svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}">' + "".join(elements) + "</svg></html>", encoding="utf-8")
    args = [
        str(chrome_path()), "--headless=new", "--disable-gpu", "--no-first-run",
        "--no-default-browser-check", "--disable-background-networking", "--hide-scrollbars",
        "--disable-extensions", "--default-background-color=00000000",
        "--force-device-scale-factor=1", "--run-all-compositor-stages-before-draw",
        "--virtual-time-budget=1000", f"--user-data-dir={RENDER / 'chrome-profile'}",
        f"--window-size={width},{height}", f"--screenshot={shot}", page.as_uri(),
    ]
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60,
                            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    if result.returncode or not shot.is_file():
        raise RuntimeError(f"SVG rendering failed: {result.stderr.decode(errors='replace')[-2000:]}")
    with Image.open(shot) as source_sheet:
        rgba = source_sheet.convert("RGBA")
        if rgba.size != (width, height):
            raise RuntimeError(f"Unexpected render size {rgba.size}, expected {(width, height)}")
        for x, y, size, output in placements:
            output.parent.mkdir(parents=True, exist_ok=True)
            rgba.crop((x, y, x + size, y + size)).save(output, optimize=True)


def make_ico(path: Path, images: list[Path]) -> None:
    entries = []
    payloads = []
    offset = 6 + 16 * len(images)
    for image in images:
        data = image.read_bytes()
        with Image.open(image) as png:
            width, height = png.size
            if width != height or width > 256 or png.mode != "RGBA":
                raise ValueError(f"Invalid ICO source {image}")
        entries.append(struct.pack("<BBBBHHII", width % 256, height % 256, 0, 0, 1, 32, len(data), offset))
        payloads.append(data)
        offset += len(data)
    path.write_bytes(struct.pack("<HHH", 0, 1, len(images)) + b"".join(entries) + b"".join(payloads))


def font(size: int) -> ImageFont.FreeTypeFont:
    candidates = [Path(r"C:\Windows\Fonts\segoeui.ttf"), Path(r"C:\Windows\Fonts\arial.ttf")]
    for path in candidates:
        if path.is_file():
            return ImageFont.truetype(str(path), size)
    return ImageFont.load_default(size=size)


def comparison_sheet(version: str, sources: list[tuple[str, Path]], output: Path) -> None:
    jobs = []
    for name, path in sources:
        for size in (128, 32, 16):
            jobs.append((path, size, RENDER / f"{name}-{version}-{size}.png"))
    render_at_sizes(jobs)
    sheet = Image.new("RGB", (960, 480), "#F3F4F6")
    draw = ImageDraw.Draw(sheet)
    draw.text((32, 20), "OpenHinoVoice / icon directions", font=font(25), fill="#20232A")
    draw.text((32, 57), "Vector render / 128, 32 and 16 px / light and dark backgrounds", font=font(14), fill="#5D6570")
    for col, (name, _) in enumerate(sources):
        left = 24 + 312 * col
        draw.rounded_rectangle((left, 96, left + 288, 456), radius=18, fill="#FFFFFF")
        draw.text((left + 20, 112), name, font=font(20), fill="#20232A")
        with Image.open(RENDER / f"{name}-{version}-128.png") as mark:
            sheet.paste(mark, (left + 80, 155), mark)
        for row, (background, label) in enumerate([("#FFFFFF", "Light"), ("#20232A", "Dark")]):
            top = 306 + row * 64
            draw.rectangle((left + 12, top, left + 276, top + 56), fill=background)
            draw.text((left + 26, top + 16), label, font=font(14), fill="#616874" if row == 0 else "#D9DCE1")
            for size, x_offset in [(32, 138), (16, 216)]:
                with Image.open(RENDER / f"{name}-{version}-{size}.png") as mark:
                    sheet.paste(mark, (left + x_offset, top + (56 - size) // 2), mark)
    sheet.save(output, optimize=True)



def production_assets() -> dict:
    selected = ASSETS / "concepts" / "b-hwave-v2.svg"
    selected_body = inner_svg(selected.read_text(encoding="utf-8"))
    write_svg(ASSETS / "source.svg", "OpenHinoVoice", selected_body[selected_body.index("</title>") + 8:])
    source = ET.fromstring(selected.read_text(encoding="utf-8"))
    paths = source.findall("{http://www.w3.org/2000/svg}path")
    outer, glyph = (path.attrib["d"] for path in paths)
    write_svg(ASSETS / "monochrome.svg", "OpenHinoVoice monochrome",
              f'<path fill="#20232A" fill-rule="evenodd" d="{outer} {glyph}"/>')
    write_svg(ASSETS / "reversed.svg", "OpenHinoVoice reversed",
              f'<path fill="#FFFFFF" fill-rule="evenodd" d="{outer} {glyph}"/>')
    wave = (
        "M80 104A16 16 0 0 1 96 120V168A16 16 0 0 1 64 168V120A16 16 0 0 1 80 104Z "
        "M128 56A16 16 0 0 1 144 72V184A16 16 0 0 1 112 184V72A16 16 0 0 1 128 56Z "
        "M176 88A16 16 0 0 1 192 104V168A16 16 0 0 1 160 168V104A16 16 0 0 1 176 88Z"
    )
    hourglass = "M72 64H184V88L144 128L184 168V192H72V168L112 128L72 88Z"
    error = "M112 56H144V144H112Z M112 168H144V200H112Z"
    symbols = {"idle": glyph, "recording": wave, "processing": hourglass, "error": error}
    palette = {
        "light": {"idle": "#33363D", "recording": RED, "processing": "#596471", "error": "#A64A00"},
        "dark": {"idle": "#E7EAF0", "recording": "#FF8094", "processing": "#BFC7D2", "error": "#FFBD66"},
    }
    jobs = [(ASSETS / "source.svg", size, RENDER / f"app-{size}.png") for size in SIZES]
    tray_map = {}
    for theme, colors in palette.items():
        tray_map[theme] = {}
        for state, symbol in symbols.items():
            stem = f"{theme}-{state}"
            path = ASSETS / "tray" / f"{stem}.svg"
            write_svg(path, f"OpenHinoVoice {state} ({theme} background)",
                      f'<path fill="{colors[state]}" fill-rule="evenodd" d="{outer} {symbol}"/>')
            for size in TRAY_SIZES:
                jobs.append((path, size, RENDER / f"{stem}-{size}.png"))
            tray_map[theme][state] = f"tray/{stem}.ico"
    render_at_sizes(jobs)
    make_ico(ASSETS / "app.ico", [RENDER / f"app-{size}.png" for size in SIZES])
    shutil.copyfile(RENDER / "app-256.png", ASSETS / "settings.png")
    for theme in palette:
        for state in symbols:
            stem = f"{theme}-{state}"
            make_ico(ASSETS / "tray" / f"{stem}.ico", [RENDER / f"{stem}-{size}.png" for size in TRAY_SIZES])
            shutil.copyfile(RENDER / f"{stem}-32.png", ASSETS / "tray" / f"{stem}.png")
    preview = Image.new("RGB", (960, 336), "#F3F4F6")
    draw = ImageDraw.Draw(preview)
    draw.text((24, 14), "Selected: H wave / tray states", font=font(24), fill="#20232A")
    for row, theme in enumerate(("light", "dark")):
        y = 64 + row * 128
        bg, fg = ("#FFFFFF", "#33363D") if theme == "light" else ("#20232A", "#E7EAF0")
        draw.rounded_rectangle((16, y, 944, y + 112), radius=12, fill=bg)
        draw.text((32, y + 12), theme, font=font(17), fill=fg)
        for col, state in enumerate(symbols):
            x = 176 + col * 190
            draw.text((x, y + 12), state, font=font(16), fill=fg)
            for size, offset in ((32, 0), (16, 68)):
                with Image.open(RENDER / f"{theme}-{state}-{size}.png") as icon:
                    preview.paste(icon, (x + offset, y + 64 - size // 2), icon)
    preview.save(ASSETS / "concepts" / "tray-comparison.png", optimize=True)
    runtime = [ASSETS / "source.svg", ASSETS / "monochrome.svg", ASSETS / "reversed.svg",
               ASSETS / "app.ico", ASSETS / "settings.png"]
    runtime.extend(ASSETS / "tray" / f"{theme}-{state}.{extension}"
                   for theme in palette for state in symbols for extension in ("svg", "ico", "png"))
    files = []
    for path in runtime:
        data = path.read_bytes()
        entry = {"path": path.relative_to(ASSETS).as_posix(), "bytes": len(data),
                 "sha256": hashlib.sha256(data).hexdigest(), "format": path.suffix[1:]}
        if path.suffix == ".ico":
            entry["sizes"] = list(SIZES if path.name == "app.ico" else TRAY_SIZES)
        elif path.suffix == ".png":
            with Image.open(path) as img:
                entry.update(width=img.width, height=img.height, mode=img.mode)
        else:
            entry["viewBox"] = [0, 0, 256, 256]
        files.append(entry)
    manifest = {
        "schema_version": 1,
        "product": "OpenHinoVoice",
        "selected_concept": "b-hwave-v2",
        "source": "source.svg",
        "app_icon": "app.ico",
        "settings_icon": "settings.png",
        "tray_icons": tray_map,
        "palette": {"brand": RED, "tray": palette},
        "state_shapes": {"idle": "H with rising voice stroke", "recording": "three voice bars",
                         "processing": "hourglass", "error": "exclamation mark"},
        "runtime_asset_bytes": sum(entry["bytes"] for entry in files),
        "runtime_budget_bytes": 128 * 1024,
        "files": files,
    }
    if manifest["runtime_asset_bytes"] > manifest["runtime_budget_bytes"]:
        raise RuntimeError("Branding asset budget exceeded")
    (ASSETS / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
                                         encoding="utf-8", newline="\n")
    return manifest



def verify_assets() -> dict:
    import io
    manifest = json.loads((ASSETS / "manifest.json").read_text(encoding="utf-8"))
    count = frame_count = measured_bytes = 0
    alpha_states = {}
    for entry in manifest["files"]:
        path = ASSETS / entry["path"]
        data = path.read_bytes()
        if len(data) != entry["bytes"] or hashlib.sha256(data).hexdigest() != entry["sha256"]:
            raise ValueError(f"Manifest mismatch: {path}")
        measured_bytes += len(data)
        count += 1
        if path.suffix == ".svg":
            root = ET.fromstring(data)
            forbidden = {"text", "image", "filter", "mask", "script", "foreignObject"}
            if any(element.tag.rsplit("}", 1)[-1] in forbidden for element in root.iter()):
                raise ValueError(f"Non-portable SVG content: {path}")
            if root.attrib.get("viewBox") != "0 0 256 256":
                raise ValueError(f"Wrong SVG viewBox: {path}")
        elif path.suffix == ".png":
            with Image.open(io.BytesIO(data)) as png:
                if png.mode != "RGBA" or png.size != (entry["width"], entry["height"]):
                    raise ValueError(f"Wrong PNG format: {path}")
                if png.getpixel((0, 0))[3] != 0:
                    raise ValueError(f"PNG background is not transparent: {path}")
        elif path.suffix == ".ico":
            reserved, kind, number = struct.unpack_from("<HHH", data)
            if (reserved, kind, number) != (0, 1, len(entry["sizes"])):
                raise ValueError(f"Invalid ICO header: {path}")
            for i, size in enumerate(entry["sizes"]):
                w, h, _, reserved, planes, bits, length, offset = struct.unpack_from("<BBBBHHII", data, 6 + 16 * i)
                if (w or 256, h or 256, reserved, planes, bits) != (size, size, 0, 1, 32):
                    raise ValueError(f"Invalid ICO entry: {path}, {size}")
                frame = data[offset:offset + length]
                if len(frame) != length or not frame.startswith(b"\x89PNG\r\n\x1a\n"):
                    raise ValueError(f"Truncated ICO payload: {path}, {size}")
                with Image.open(io.BytesIO(frame)) as png:
                    if png.size != (size, size) or png.mode != "RGBA":
                        raise ValueError(f"Invalid ICO PNG frame: {path}, {size}")
                    alpha = png.getchannel("A")
                    if alpha.getextrema() != (0, 255):
                        raise ValueError(f"ICO needs transparent and opaque pixels: {path}, {size}")
                    if size == 16 and path.parent.name == "tray":
                        alpha_states[path.stem] = hashlib.sha256(alpha.tobytes()).hexdigest()
                frame_count += 1
    if measured_bytes != manifest["runtime_asset_bytes"] or measured_bytes > manifest["runtime_budget_bytes"]:
        raise ValueError("Runtime asset budget mismatch")
    for theme in ("light", "dark"):
        states = {alpha_states[f"{theme}-{state}"] for state in ("idle", "recording", "processing", "error")}
        if len(states) != 4:
            raise ValueError("Tray states are not distinguished by shape")
    return {"status": "passed", "asset_files": count, "ico_frames": frame_count,
            "runtime_asset_bytes": measured_bytes, "tray_state_silhouettes_per_theme": 4}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--concepts-only", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.check:
        print(json.dumps(verify_assets()))
        return
    sources = [(name, ASSETS / "concepts" / path) for name, path in [
        ("A / Voice", "a-voice-v1.svg"), ("B / H wave", "b-hwave-v1.svg"), ("C / Speech", "c-text-v1.svg"),
    ]]
    sources = [(name.replace("/", "-"), path) for name, path in sources]
    comparison_sheet("v1", sources, ASSETS / "concepts" / "comparison-v1.png")
    if args.concepts_only:
        print(json.dumps({"concept_sheet": str(ASSETS / "concepts" / "comparison-v1.png")}))
        return
    sources[1] = ("B - H wave", ASSETS / "concepts" / "b-hwave-v2.svg")
    comparison_sheet("v2", sources, ASSETS / "concepts" / "comparison-v2.png")
    manifest = production_assets()
    print(json.dumps(verify_assets()))


if __name__ == "__main__":
    main()
