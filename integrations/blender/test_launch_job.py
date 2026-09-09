#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Ce que le manifeste d'un run a le droit de contenir.

    python3 -m unittest discover -s integrations/blender -p 'test_*.py'

L'archive d'un run est durable et partagée : c'est ce qui la rend utile — on
compare deux rendus des semaines plus tard sans qu'aucun pod n'existe encore —
et c'est ce qui la rend dangereuse. Un jeton qui y entre n'en sort plus.
"""

import importlib.util
import pathlib
import unittest

# Chargé par chemin : `launch_job.py` est un script, pas un paquet.
_spec = importlib.util.spec_from_file_location(
    "launch_job", pathlib.Path(__file__).with_name("launch_job.py"))
assert _spec is not None and _spec.loader is not None, "launch_job.py introuvable"
launch_job = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(launch_job)


class Redaction(unittest.TestCase):
    """Le manifeste ne doit jamais porter de secret."""

    def test_the_ion_token_never_reaches_the_archive(self):
        env = {"TUILE_ION_TOKEN": "eyJhbGciOi.SECRET.PAYLOAD",
               "JOB_FRAMES": "1:48"}
        out = launch_job.redacted(env)
        self.assertEqual(out["TUILE_ION_TOKEN"], "<expurgé>")
        self.assertEqual(out["JOB_FRAMES"], "1:48",
                         "ce qui n'est pas un secret doit rester lisible")
        self.assertNotIn("SECRET", repr(out))

    def test_every_named_secret_is_covered(self):
        env = {k: "valeur-secrète" for k in launch_job.SECRET_KEYS}
        out = launch_job.redacted(env)
        self.assertNotIn("valeur-secrète", repr(out))

    def test_a_presigned_url_keeps_its_path_and_loses_its_signature(self):
        """Une URL présignée autorise l'écriture : c'est un secret à durée
        limitée. Le chemin reste, pour savoir où le run a déposé."""
        url = ("https://acct.r2.cloudflarestorage.com/bucket/renders/r1/render.mp4"
               "?X-Amz-Signature=deadbeef&X-Amz-Expires=604800")
        out = launch_job.redacted({"JOB_UPLOAD_PUT_URL": url})
        self.assertIn("renders/r1/render.mp4", out["JOB_UPLOAD_PUT_URL"])
        self.assertNotIn("deadbeef", out["JOB_UPLOAD_PUT_URL"])


class RunIdentity(unittest.TestCase):
    def test_sorting_by_name_sorts_by_date_and_shows_the_commit(self):
        early = launch_job.run_id({"short": "abc1234", "dirty": False})
        later = launch_job.run_id({"short": "abc1234", "dirty": False})
        self.assertIn("abc1234", early)
        self.assertNotIn("dirty", early)
        # Deux runs de la même seconde ne doivent pas écraser leurs archives.
        self.assertNotEqual(early, later)
        # L'horodatage d'abord, donc l'ordre alphabétique est l'ordre du temps.
        self.assertTrue(early[:8].isdigit(), early)

    def test_a_dirty_tree_says_so(self):
        """Une image produite depuis un arbre modifié ne correspond à aucun
        commit ; le nom du run doit le dire, sinon l'archive ment."""
        self.assertIn("-dirty", launch_job.run_id({"short": "abc1234",
                                                   "dirty": True}))

    def test_without_git_the_run_is_still_named(self):
        self.assertIn("nogit", launch_job.run_id({}))


if __name__ == "__main__":
    unittest.main()
