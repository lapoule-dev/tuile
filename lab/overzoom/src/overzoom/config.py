# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Where things are, for one lab: read from a JSON file kept out of the repository (``lab.json``).

    {
      "url_list": "/path/to/presigned.json",      list of {key, url, expires_at} for the zone's objects
      "data": "data",                              local working directory (archives, datasets, runs)
      "remote": "s3://bucket/prefix",              where datasets and runs are shared with the GPU machine
      "aws_profile": "name"                        optional; omitted on a machine with its own role
    }
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Lab:
    url_list: Path
    data: Path
    remote: str
    aws_profile: str | None = None

    @classmethod
    def load(cls, path: Path) -> Lab:
        raw = json.loads(path.read_text())
        base = path.parent
        return cls(url_list=(base / raw["url_list"]).resolve() if raw.get("url_list") else Path(),
                   data=(base / raw.get("data", "data")).resolve(), remote=raw["remote"].rstrip("/"),
                   aws_profile=raw.get("aws_profile"))

    def aws(self) -> list[str]:
        return ["aws"] + (["--profile", self.aws_profile] if self.aws_profile else [])

    def push(self, local: Path, remote_name: str) -> None:
        """Mirror a local directory to ``remote/remote_name``."""
        subprocess.run(self.aws() + ["s3", "sync", str(local), f"{self.remote}/{remote_name}",
                                     "--only-show-errors"], check=True)

    def put(self, local: Path, remote_name: str) -> None:
        """Copy one file to ``remote/remote_name``."""
        subprocess.run(self.aws() + ["s3", "cp", str(local), f"{self.remote}/{remote_name}", "--only-show-errors"],
                       check=True)

    def pull(self, remote_name: str, local: Path, include: list[str] | None = None) -> None:
        """Mirror ``remote/remote_name`` here; with ``include``, only the keys matching those patterns."""
        local.mkdir(parents=True, exist_ok=True)
        only = ["--exclude", "*", *[a for p in include for a in ("--include", p)]] if include else []
        subprocess.run(self.aws() + ["s3", "sync", f"{self.remote}/{remote_name}", str(local), *only,
                                     "--only-show-errors"], check=True)
