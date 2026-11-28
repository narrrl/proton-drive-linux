#!/usr/bin/env python3
"""Write the tray's symbolic status icons to crates/pdfs-gui/resources/icons/tray/.

Each icon is the app icon's folder, reduced to one colour on a 16 px grid,
with a small badge for the state in a bite out of its lower right corner.

Panels recolour symbolic icons, and several do it by forcing the fill of
every path, rect and circle. So every shape here is a filled path in one
colour: no strokes, masks or opacity. Holes come from the even-odd rule.

Run it again after changing a shape: python3 -I scripts/render-tray-icons.py
"""

import math
import pathlib

OUT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "crates" / "pdfs-gui" / "resources" / "icons" / "tray"
)
PREFIX = "io.narl.proton-drive-linux-tray"

# The badge sits on a circle of radius BITE around (CX, CY), and the folder
# keeps a one-pixel gap from it.
CX, CY, BITE = 12.0, 12.0, 4.5


def f(v):
    return f"{v:.3f}".rstrip("0").rstrip(".")


def pt(x, y):
    return f"{f(x)} {f(y)}"


def polar(r, deg):
    a = math.radians(deg)
    return CX + r * math.cos(a), CY + r * math.sin(a)


def circle(cx, cy, r):
    return (
        f"M{pt(cx - r, cy)}A{f(r)} {f(r)} 0 1 0 {pt(cx + r, cy)}"
        f"A{f(r)} {f(r)} 0 1 0 {pt(cx - r, cy)}Z"
    )


def rounded_rect(x0, y0, x1, y1, r):
    return (
        f"M{pt(x0 + r, y0)}H{f(x1 - r)}Q{pt(x1, y0)} {pt(x1, y0 + r)}"
        f"V{f(y1 - r)}Q{pt(x1, y1)} {pt(x1 - r, y1)}H{f(x0 + r)}"
        f"Q{pt(x0, y1)} {pt(x0, y1 - r)}V{f(y0 + r)}Q{pt(x0, y0)} {pt(x0 + r, y0)}Z"
    )


def bite_from_right(x, y_bottom):
    """The bite's arc, from where it crosses the edge x = `x` to where it
    crosses the edge y = `y_bottom`, going round its upper left."""
    y = CY - math.sqrt(BITE**2 - (x - CX) ** 2)
    bx = CX - math.sqrt(BITE**2 - (y_bottom - CY) ** 2)
    return y, f"A{f(BITE)} {f(BITE)} 0 0 0 {pt(bx, y_bottom)}"


# The back of the folder: its tab, and the band the front flap leaves.
# Its lower edge follows the front flap's top 0.75 px higher.
BACK = (
    "M1 3.5Q1 2.5 2 2.5H5.6Q6.1 2.5 6.45 2.85L7.4 3.8Q7.6 4 7.9 4"
    "H14Q15 4 15 5V6.75H7.05L5.8 5.5H1Z"
)
# The front flap has a tab of its own, as in the app icon, so the two
# flaps read as the app's folder rather than a stock one.
FRONT_TOP = (
    "M1 7.25Q1 6.25 2 6.25H5.2Q5.6 6.25 5.9 6.55L6.6 7.25Q6.85 7.5 7.2 7.5"
    "H14Q15 7.5 15 8.5"
)
RIGHT, BOTTOM = 15.0, 13.5


def front(bitten):
    if not bitten:
        return FRONT_TOP + "V12.5Q15 13.5 14 13.5H2Q1 13.5 1 12.5Z"
    y, arc = bite_from_right(RIGHT, BOTTOM)
    return FRONT_TOP + f"V{f(y)}{arc}H2Q1 13.5 1 12.5Z"


def front_hole(bitten):
    """The inside of a hollow front flap: a 1.25 px wall below its tab."""
    x0, y0, x1, y1 = 2.25, 8.75, 13.75, 12.25
    if not bitten:
        return f"M{pt(x0, y0)}H{f(x1)}V{f(y1)}H{f(x0)}Z"
    # The bite crosses the top of the hole, not its right side.
    tx = CX - math.sqrt(BITE**2 - (y0 - CY) ** 2)
    bx = CX - math.sqrt(BITE**2 - (y1 - CY) ** 2)
    return (
        f"M{pt(x0, y0)}H{f(tx)}A{f(BITE)} {f(BITE)} 0 0 0 {pt(bx, y1)}"
        f"H{f(x0)}Z"
    )


def arrow(start, end, inner=1.9, outer=3.1, head=0.85):
    """A thick arc from `start` to `end` degrees, clockwise on screen, with an
    arrowhead at `end`."""
    mid = (inner + outer) / 2
    steps = 8
    out = [polar(outer, start + (end - start) * i / steps) for i in range(steps + 1)]
    inn = [polar(inner, end - (end - start) * i / steps) for i in range(steps + 1)]
    tip = polar(mid, end + 42)
    head_out = polar(outer + head, end)
    head_in = polar(inner - head, end)
    points = out + [head_out, tip, head_in] + inn
    return "M" + "L".join(pt(*p) for p in points) + "Z"


def bars(points):
    return "M" + "L".join(pt(CX + x, CY + y) for x, y in points) + "Z"


def cross(length=2.6, width=0.65):
    a, w = length / math.sqrt(2), width / math.sqrt(2)
    # A plus sign turned 45 degrees: twelve corners.
    pts = [
        (0, -2 * w), (a - w, -a - w), (a + w, -a + w), (2 * w, 0),
        (a + w, a - w), (a - w, a + w), (0, 2 * w), (-a + w, a + w),
        (-a - w, a - w), (-2 * w, 0), (-a - w, -a + w), (-a + w, -a - w),
    ]
    return bars(pts)


def lock():
    body = rounded_rect(9.5, 11.6, 14.5, 14.8, 0.6)
    # The shackle: half a ring, open at the bottom where it meets the body.
    ro, ri, cy = 1.9, 1.05, 11.6
    shackle = (
        f"M{pt(CX - ro, cy)}V{f(cy - 0.1)}A{f(ro)} {f(ro)} 0 0 1 {pt(CX + ro, cy - 0.1)}"
        f"V{f(cy)}H{f(CX + ri)}V{f(cy - 0.1)}A{f(ri)} {f(ri)} 0 0 0 {pt(CX - ri, cy - 0.1)}"
        f"V{f(cy)}Z"
    )
    return shackle + body


# A disc with the badge's mark cut out, for the states that need a look.
def disc_with(mark):
    return circle(CX, CY, 3.5) + mark


BADGES = {
    "syncing": arrow(200, 320) + arrow(20, 140),
    "paused": rounded_rect(9.7, 9.25, 11.2, 14.75, 0.4)
    + rounded_rect(12.8, 9.25, 14.3, 14.75, 0.4),
    "attention": disc_with(
        rounded_rect(11.3, 9.5, 12.7, 12.6, 0.5) + circle(12, 14.05, 0.75)
    ),
    "session-expired": lock(),
    "disconnected": cross(),
}

# State: (hollow front flap, badge).
ICONS = {
    "synced": (False, None),
    "syncing": (False, "syncing"),
    "paused": (False, "paused"),
    "attention": (False, "attention"),
    "session-expired": (False, "session-expired"),
    "offline": (True, None),
    "disconnected": (True, "disconnected"),
}

TEMPLATE = """<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
<!-- Generated by scripts/render-tray-icons.py; edit that, not this. -->
<style>.ColorScheme-Text{{color:#2e3436}}</style>
<path class="ColorScheme-Text" fill="currentColor" fill-rule="evenodd" d="{d}"/>
</svg>
"""


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for name, (hollow, badge) in ICONS.items():
        bitten = badge is not None
        d = BACK + front(bitten)
        if hollow:
            d += front_hole(bitten)
        if badge:
            d += BADGES[badge]
        path = OUT / f"{PREFIX}-{name}-symbolic.svg"
        path.write_text(TEMPLATE.format(d=d))
        print(path.relative_to(OUT.parents[4]))


if __name__ == "__main__":
    main()
