"""Rasterize the simple rectangle-only source SVG. Build-time tool; no runtime Python."""
from pathlib import Path
import xml.etree.ElementTree as ET
from PIL import Image, ImageDraw

assets = Path(__file__).resolve().parents[1] / "assets"
root = ET.parse(assets / "icon.svg").getroot()
scale = 16
image = Image.new("RGBA", (64 * scale, 64 * scale))
draw = ImageDraw.Draw(image)
for node in root:
    assert node.tag.endswith("}rect"), "Only rectangle primitives are supported"
    x, y, w, h, r = [float(node.attrib.get(k, 0)) for k in ("x", "y", "width", "height", "rx")]
    draw.rounded_rectangle((x * scale, y * scale, (x + w) * scale - 1, (y + h) * scale - 1),
                           radius=r * scale, fill=node.attrib["fill"])
icon = image.resize((256, 256), Image.Resampling.LANCZOS)
icon.save(assets / "icon.png")
image.resize((32, 32), Image.Resampling.LANCZOS).save(assets / "icon-32.png")
icon.save(assets / "icon.ico", sizes=[(s, s) for s in (16, 20, 24, 32, 48, 64, 128, 256)])
icon.save(assets / "icon.icns", sizes=[(s, s) for s in (16, 32, 64, 128, 256)])
