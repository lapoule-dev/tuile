# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Génère le layer de look « terrain » pour un manifeste TuileGlobe.

Le soleil n'est pas un angle choisi à la main : il dépend d'une HEURE au-dessus
de CE point du globe. Le script lit `primvars:tuile:renderOrigin` dans le
manifeste (ECEF → lat/lon), calcule azimut et élévation solaires (approximation
NOAA, ~0,1°) pour la date-heure demandée, et écrit un layer qui sublaye le
manifeste avec un DistantLight orienté en conséquence, un DomeLight HDRI aligné
sur le zénith local, et l'over caméra (focale).

    python3 make_look.py <manifest.usda> <out.usda> \
        [--at 2026-09-08T17:30] [--utc-offset 2] [--focal 47] \
        [--sky ./sky-day.exr] [--sun-intensity 5.0]

Sous l'horizon (élévation ≤ 0), le script refuse : une nuit se compose
autrement qu'en poussant un soleil sous le sol.
"""

import argparse
import math
import re
import sys
from datetime import datetime


def solar_position(lat_deg, lon_deg, when, utc_offset_hours):
    """Azimut (depuis le nord, vers l'est) et élévation solaires, en degrés.

    L'approximation NOAA « General Solar Position Calculations » : largement
    assez précise pour éclairer un rendu, et sans dépendance.
    """
    day_of_year = when.timetuple().tm_yday
    hours = when.hour + when.minute / 60.0 + when.second / 3600.0
    gamma = 2.0 * math.pi / 365.0 * (day_of_year - 1 + (hours - 12) / 24.0)

    eqtime = 229.18 * (
        0.000075
        + 0.001868 * math.cos(gamma)
        - 0.032077 * math.sin(gamma)
        - 0.014615 * math.cos(2 * gamma)
        - 0.040849 * math.sin(2 * gamma)
    )
    decl = (
        0.006918
        - 0.399912 * math.cos(gamma)
        + 0.070257 * math.sin(gamma)
        - 0.006758 * math.cos(2 * gamma)
        + 0.000907 * math.sin(2 * gamma)
        - 0.002697 * math.cos(3 * gamma)
        + 0.00148 * math.sin(3 * gamma)
    )

    time_offset = eqtime + 4.0 * lon_deg - 60.0 * utc_offset_hours
    true_solar_minutes = hours * 60.0 + time_offset
    hour_angle = math.radians(true_solar_minutes / 4.0 - 180.0)

    lat = math.radians(lat_deg)
    cos_zenith = math.sin(lat) * math.sin(decl) + math.cos(lat) * math.cos(
        decl
    ) * math.cos(hour_angle)
    zenith = math.acos(max(-1.0, min(1.0, cos_zenith)))
    elevation = 90.0 - math.degrees(zenith)

    cos_az = (math.sin(lat) * cos_zenith - math.sin(decl)) / (
        math.cos(lat) * math.sin(zenith)
    )
    azimuth = math.degrees(math.acos(max(-1.0, min(1.0, cos_az))))
    if hour_angle > 0:
        azimuth = 360.0 - (180.0 - azimuth)
    else:
        azimuth = 180.0 - azimuth
    return azimuth % 360.0, elevation


def norm(v):
    n = math.sqrt(sum(c * c for c in v)) or 1.0
    return tuple(c / n for c in v)


def cross(a, b):
    return (
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    )


def matrix_rows(x, y, z):
    row = lambda v: f"({v[0]}, {v[1]}, {v[2]}, 0)"
    return f"( {row(x)}, {row(y)}, {row(z)}, (0, 0, 0, 1) )"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("manifest")
    p.add_argument("out")
    p.add_argument("--at", default="2026-06-21T10:00",
                   help="date-heure locale ISO (défaut : solstice, 10h)")
    p.add_argument("--utc-offset", type=float, default=2.0)
    p.add_argument("--focal", type=float, default=47.0)
    p.add_argument("--sky", default="./sky-day.exr",
                   help="HDRI équirect .exr (chemin relatif au layer)")
    p.add_argument("--sun-intensity", type=float, default=5.0)
    args = p.parse_args()

    text = open(args.manifest).read()
    m = re.search(
        r"tuile:renderOrigin\s*=\s*\(([^,]+),([^,]+),([^)]+)\)", text
    )
    if not m:
        sys.exit("le manifeste ne porte pas primvars:tuile:renderOrigin")
    origin = tuple(float(g) for g in m.groups())

    zen = norm(origin)
    east = norm(cross((0.0, 0.0, 1.0), zen))
    north = cross(zen, east)
    lat = math.degrees(math.asin(zen[2]))
    lon = math.degrees(math.atan2(origin[1], origin[0]))

    when = datetime.fromisoformat(args.at)
    azimuth, elevation = solar_position(lat, lon, when, args.utc_offset)
    if elevation <= 0.5:
        sys.exit(
            f"soleil sous l'horizon à {args.at} ({elevation:.1f}°) — "
            "choisis une heure de jour"
        )

    el, az = math.radians(elevation), math.radians(azimuth)
    # Azimut NOAA : depuis le nord, vers l'est. Direction d'éclairage d :
    # du ciel vers le sol.
    horizontal = tuple(
        math.cos(az) * north[i] + math.sin(az) * east[i] for i in range(3)
    )
    d = tuple(
        -math.sin(el) * zen[i] + math.cos(el) * (-horizontal[i])
        for i in range(3)
    )
    z_sun = tuple(-c for c in d)
    x_sun = norm(cross(zen, z_sun))
    y_sun = cross(z_sun, x_sun)

    manifest_ref = args.manifest if "/" in args.manifest else f"./{args.manifest}"
    layer = f"""#usda 1.0
(
    \"\"\"
    Look « terrain » généré par make_look.py — NE PAS éditer les matrices à
    la main, régénérer.
    Manifeste : {args.manifest}
    Site : lat {lat:.4f}°, lon {lon:.4f}°
    Soleil : {args.at} (UTC{args.utc_offset:+.1f}) → azimut {azimuth:.1f}°,
    élévation {elevation:.1f}°
    Rendu : usdrecord --disableCameraLight --camera /World/ShotCam …
    \"\"\"
    subLayers = [
        @{manifest_ref}@
    ]
)

over "World"
{{
    over "ShotCam"
    {{
        float focalLength = {args.focal}
    }}

    def DomeLight "Sky"
    {{
        float inputs:intensity = 1.0
        asset inputs:texture:file = @{args.sky}@
        matrix4d xformOp:transform = {matrix_rows(east, north, zen)}
        uniform token[] xformOpOrder = ["xformOp:transform"]
    }}

    def DistantLight "Sun"
    {{
        float inputs:intensity = {args.sun_intensity}
        color3f inputs:color = (1.0, 0.98, 0.94)
        float inputs:angle = 0.53
        matrix4d xformOp:transform = {matrix_rows(x_sun, y_sun, z_sun)}
        uniform token[] xformOpOrder = ["xformOp:transform"]
    }}
}}
"""
    open(args.out, "w").write(layer)
    print(
        f"{args.out}: soleil {args.at} → az {azimuth:.1f}°, él {elevation:.1f}°"
        f" (site {lat:.3f}, {lon:.3f})"
    )


if __name__ == "__main__":
    main()
