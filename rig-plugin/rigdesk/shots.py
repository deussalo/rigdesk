"""desk/screenshot answers with the image itself, so the agent sees the screen in the same call instead of reading a kept file after it."""
from __future__ import annotations

import base64
import io
from dataclasses import dataclass

from PIL import Image

MAX_EDGE = 2000
JPEG_QUALITY = 80


@dataclass(frozen=True)
class Inline:
    data: str
    mime: str
    note: str


def inline(png: bytes) -> Inline:
    """The smaller of the PC's PNG and a JPEG of it; a display wider than MAX_EDGE is shrunk, and the note says how to map x,y back."""
    with Image.open(io.BytesIO(png)) as opened:
        image = opened.convert("RGB")
    width, height = image.size
    scale = max(width, height) / MAX_EDGE
    if scale > 1:
        image = image.resize((max(1, round(width / scale)), max(1, round(height / scale))), Image.Resampling.LANCZOS)  # pyright: ignore[reportUnknownMemberType]
    out = io.BytesIO()
    image.save(out, "JPEG", quality=JPEG_QUALITY)
    data, mime = (png, "image/png") if scale <= 1 and len(png) <= out.tell() else (out.getvalue(), "image/jpeg")
    shown = f"[{mime.removeprefix('image/').upper()} {image.width}x{image.height}, {len(data)} bytes.]"
    mapped = f" [Displayed at {image.width}x{image.height} of {width}x{height}: multiply x,y by {width / image.width:.2f} for pointer tools.]" if scale > 1 else ""
    return Inline(base64.b64encode(data).decode("ascii"), mime, shown + mapped)
