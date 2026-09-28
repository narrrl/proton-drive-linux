#!/usr/bin/env python3
"""Regenerate crates/pdfs-gui/data/land.txt from Natural Earth's land polygons.

Usage:
    curl -LO https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_50m_land.geojson
    scripts/natural-earth-land.py ne_50m_land.geojson crates/pdfs-gui/data/land.txt

Natural Earth is in the public domain; the About dialog credits it anyway.

The output is one ring per line, as space-separated "longitude,latitude"
pairs rounded to two decimals (about a kilometre). Each ring is simplified
with Ramer-Douglas-Peucker to TOLERANCE degrees, which is finer than the map
can show at its closest zoom, and rings left with fewer than four points
(islets smaller than the tolerance) are dropped. Holes (lakes) are dropped
too: the map fills the land and draws nothing on top of it.
"""

import json
import sys

TOLERANCE = 0.04


def point_line_distance(p, a, b):
    (x, y), (x1, y1), (x2, y2) = p, a, b
    dx, dy = x2 - x1, y2 - y1
    if dx == 0 and dy == 0:
        return ((x - x1) ** 2 + (y - y1) ** 2) ** 0.5
    t = max(0, min(1, ((x - x1) * dx + (y - y1) * dy) / (dx * dx + dy * dy)))
    return ((x - x1 - t * dx) ** 2 + (y - y1 - t * dy) ** 2) ** 0.5


def simplify(points):
    """Ramer-Douglas-Peucker, iterative so long coastlines don't recurse deep."""
    keep = [False] * len(points)
    keep[0] = keep[-1] = True
    stack = [(0, len(points) - 1)]
    while stack:
        first, last = stack.pop()
        worst, index = 0.0, None
        for i in range(first + 1, last):
            d = point_line_distance(points[i], points[first], points[last])
            if d > worst:
                worst, index = d, i
        if index is not None and worst > TOLERANCE:
            keep[index] = True
            stack.append((first, index))
            stack.append((index, last))
    return [p for p, k in zip(points, keep) if k]


def main(source, target):
    features = json.load(open(source))["features"]
    lines = []
    for feature in features:
        geometry = feature["geometry"]
        polygons = (
            geometry["coordinates"]
            if geometry["type"] == "MultiPolygon"
            else [geometry["coordinates"]]
        )
        for polygon in polygons:
            ring = simplify(polygon[0])
            if len(ring) < 4:
                continue
            lines.append(" ".join(f"{lon:.2f},{lat:.2f}" for lon, lat in ring))
    with open(target, "w") as out:
        out.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
