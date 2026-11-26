"""Rasterize the native service icons with the existing GdkPixbuf SVG loader."""

import ctypes as c
from pathlib import Path


def main() -> None:
    pixbuf = c.CDLL("libgdk_pixbuf-2.0.so.0")
    pixbuf.gdk_pixbuf_new_from_file_at_scale.argtypes = [
        c.c_char_p, c.c_int, c.c_int, c.c_int, c.c_void_p,
    ]
    pixbuf.gdk_pixbuf_new_from_file_at_scale.restype = c.c_void_p
    pixbuf.gdk_pixbuf_savev.argtypes = [
        c.c_void_p, c.c_char_p, c.c_char_p, c.c_void_p, c.c_void_p, c.c_void_p,
    ]
    pixbuf.gdk_pixbuf_savev.restype = c.c_int
    gobject = c.CDLL("libgobject-2.0.so.0")
    gobject.g_object_unref.argtypes = [c.c_void_p]
    gobject.g_object_unref.restype = None

    directory = Path(__file__).resolve().parents[2] / "native_app" / "art" / "service_icons"
    for svg in sorted(directory.glob("*.svg")):
        image = pixbuf.gdk_pixbuf_new_from_file_at_scale(bytes(svg), 64, 64, True, None)
        if not image:
            raise RuntimeError(f"Could not load {svg}")
        try:
            png = svg.with_suffix(".png")
            if not pixbuf.gdk_pixbuf_savev(image, bytes(png), b"png", None, None, None):
                raise RuntimeError(f"Could not write {png}")
        finally:
            gobject.g_object_unref(image)


if __name__ == "__main__":
    main()
