# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The commercial aerial layer, through the same path as tuile's connectors (``tuile-cesium-ion``, ``tuile-bing``).

1. ion resolves asset 2 (``/v1/assets/2/endpoint``, account token as bearer) into the provider's options: its
   metadata service and a provider key;
2. the provider's metadata gives the tile template (``{subdomain}``, ``{quadkey}``) and its subdomains;
3. a tile is the template with its quadkey, asked with ``n=z`` so that a tile the provider does not have comes back
   **empty** (or marked ``X-VE-Tile-Info: no-tile``) instead of as a placeholder picture — never stored.

Secrets: the ion token is read from tuile's ``.env`` (``TUILE_ION_TOKEN``) into memory only; the provider key only
ever appears in the one metadata request. Neither is printed, logged, written to disk or put in an error message.
Tile URLs carry neither.

Use: tiles enter the dataset only as low-resolution or unlabelled inputs, and as co-located anchors next to open
orthophotos. Terms of use: accepted by the project owner for this work; flagged in the dataset's licence table.
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from pathlib import Path

import requests

ION = "https://api.cesium.com/v1/assets/2/endpoint"
ENV = Path("/Users/laurentvaldes/Projects/tuile/.env")
UA = "tuile-image-lab/0.1 (research dataset; laurent@lapoule.dev)"


def quadkey(x: int, y: int, z: int) -> str:
    out = []
    for i in range(z, 0, -1):
        mask = 1 << (i - 1)
        out.append(str((1 if x & mask else 0) + (2 if y & mask else 0)))
    return "".join(out)


def is_no_tile(tile_info: str | None, body: bytes) -> bool:
    return len(body) == 0 or (tile_info is not None and tile_info.strip().lower() == "no-tile")


def _token(env: Path = ENV) -> str:
    for line in env.read_text().splitlines():
        k, _, v = line.partition("=")
        if k.strip() == "TUILE_ION_TOKEN":
            return v.strip().strip('"').strip("'")
    raise RuntimeError("TUILE_ION_TOKEN is not in tuile's .env")


class SecretError(RuntimeError):
    """An error whose message is known to carry no secret."""


@dataclass
class Bing:
    template: str
    subdomains: list[str]
    session: requests.Session
    pause: float = 0.25
    requests_made: int = 0

    @classmethod
    def connect(cls, style: str = "Aerial", pause: float = 0.25) -> Bing:
        s = requests.Session()
        s.headers["User-Agent"] = UA
        try:
            r = s.get(ION, headers={"Authorization": f"Bearer {_token()}"}, timeout=60)
        except requests.RequestException:
            raise SecretError("ion endpoint request failed") from None
        if r.status_code != 200:
            raise SecretError(f"ion endpoint answered HTTP {r.status_code}")
        opts = r.json().get("options") or {}
        base, key = opts.get("url"), opts.get("key")
        if not base or not key:
            raise SecretError("ion endpoint has no provider url or key")
        try:
            m = s.get(f"{base.rstrip('/')}/REST/v1/Imagery/Metadata/{opts.get('mapStyle') or style}",
                      params={"incl": "ImageryProviders", "key": key, "uriScheme": "https"}, timeout=60)
        except requests.RequestException:
            # the exception's text carries the request URL, key included: never let it out
            raise SecretError("imagery metadata request failed") from None
        if m.status_code != 200:
            raise SecretError(f"imagery metadata answered HTTP {m.status_code}")
        res = m.json()["resourceSets"][0]["resources"][0]
        template = res["imageUrl"]
        template += ("&" if "?" in template else "?") + "n=z"
        return cls(template, list(res["imageUrlSubdomains"]), s, pause)

    def url(self, z: int, x: int, y: int) -> str:
        sub = self.subdomains[(x + y + z) % len(self.subdomains)]
        return self.template.replace("{subdomain}", sub).replace("{quadkey}", quadkey(x, y, z))

    def tile(self, z: int, x: int, y: int, cache: Path) -> bytes | None:
        """The tile's JPEG bytes as served, from ``cache/z/x/y.jpg`` or the network (then cached). None when the
        provider has no tile there (remembered as an empty ``.none`` file, so it is not asked again)."""
        p = cache / str(z) / str(x) / f"{y}.jpg"
        none = p.with_suffix(".none")
        if p.exists():
            return p.read_bytes()
        if none.exists():
            return None
        for attempt in range(4):
            r = self.session.get(self.url(z, x, y), timeout=60)
            self.requests_made += 1
            time.sleep(self.pause)
            if r.status_code == 200:
                break
            if r.status_code in (429, 500, 502, 503, 504):
                time.sleep(2.0 * (attempt + 1))
                continue
            break
        p.parent.mkdir(parents=True, exist_ok=True)
        if r.status_code != 200 or is_no_tile(r.headers.get("X-VE-Tile-Info"), r.content):
            none.touch()
            return None
        p.write_bytes(r.content)
        return r.content
