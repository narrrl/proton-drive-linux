#!/usr/bin/env python3
"""Regenerate crates/pdfs-core/data/cities15000.tsv from a GeoNames dump.

Usage:
    curl -LO https://download.geonames.org/export/dump/cities15000.zip
    unzip cities15000.zip
    scripts/geonames-cities.py cities15000.txt crates/pdfs-core/data/cities15000.tsv

GeoNames publishes the dump under CC BY 4.0; the About dialog credits it.

The output keeps id, name, latitude, longitude and country code, sorted by id.
Two kinds of rows are dropped, so that a photo lands in the town people would
name rather than in a part of it:

- Feature codes for sections of a city (PPLX, such as Kreuzberg) and for
  places that no longer exist (PPLH, PPLQ, PPLW).
- Districts that GeoNames lists as towns of their own but that are named after
  a town at least five times their size within 20 km in the same country,
  such as "Paris 15 Vaugirard" or "Marseille 01".
"""

import math
import sys

SKIPPED_FEATURES = {"PPLX", "PPLH", "PPLQ", "PPLW"}
DISTRICT_RADIUS_KM = 20
DISTRICT_RATIO = 5


def distance_km(a, b):
    lat1, lat2 = math.radians(a[0]), math.radians(b[0])
    dlon = math.radians(b[1] - a[1])
    h = (
        math.sin((lat2 - lat1) / 2) ** 2
        + math.cos(lat1) * math.cos(lat2) * math.sin(dlon / 2) ** 2
    )
    return 2 * 6371 * math.asin(min(1, math.sqrt(h)))


def main(source, target):
    towns = []
    with open(source, encoding="utf-8") as dump:
        for line in dump:
            f = line.rstrip("\n").split("\t")
            if f[7] in SKIPPED_FEATURES:
                continue
            towns.append(
                {
                    "id": int(f[0]),
                    "name": f[1],
                    "at": (float(f[4]), float(f[5])),
                    "country": f[8],
                    "population": int(f[14] or 0),
                }
            )

    cells = {}
    for town in towns:
        key = (math.floor(town["at"][0]), math.floor(town["at"][1]))
        cells.setdefault(key, []).append(town)

    def is_district(town):
        row, column = math.floor(town["at"][0]), math.floor(town["at"][1])
        for dlat in (-1, 0, 1):
            for dlon in (-1, 0, 1):
                for other in cells.get((row + dlat, column + dlon), []):
                    if (
                        other is not town
                        and other["country"] == town["country"]
                        and town["name"].startswith(other["name"] + " ")
                        and other["population"] >= DISTRICT_RATIO * town["population"]
                        and distance_km(town["at"], other["at"]) <= DISTRICT_RADIUS_KM
                    ):
                        return True
        return False

    kept = sorted((t for t in towns if not is_district(t)), key=lambda t: t["id"])
    with open(target, "w", encoding="utf-8") as out:
        for t in kept:
            lat, lon = t["at"]
            out.write(f"{t['id']}\t{t['name']}\t{lat:.4f}\t{lon:.4f}\t{t['country']}\n")
    print(f"{len(kept)} towns written", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
