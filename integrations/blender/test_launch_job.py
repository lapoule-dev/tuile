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
import sys
import unittest

# Jamais de bytecode pour ce que ce fichier teste.
#
# `importlib` réutilise un `.pyc` quand la source a la même taille ET la même
# seconde de mtime. Mesuré ici : remplacer `GPU-HOST-BROKEN` par
# `GPU-HOST-BORKED` — six lettres contre six lettres, dans la même seconde —
# laissait le test lire l'ancien bytecode et affirmer le contraire de ce que
# le fichier disait. Un test qui lit une version périmée de ce qu'il teste est
# pire que pas de test : il rassure.
sys.dont_write_bytecode = True

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


class Archive(unittest.TestCase):
    """Ce qu'un run dépose, et pourquoi rien n'est optionnel."""

    def test_four_objects_are_named_and_the_logs_are_among_them(self):
        """Le run dont on a le plus besoin des journaux est celui qui a
        échoué — donc les journaux ne peuvent pas dépendre de la réussite,
        ni d'un drapeau."""
        self.assertEqual(
            set(launch_job.ARCHIVE_OBJECTS),
            {"render.mp4", "logs.tar.gz", "trace.tar.gz", "profile.tar.gz"})

    def test_nothing_can_switch_the_archive_off(self):
        """`--no-archive` a coûté un rendu de 60 secondes : le pod a été
        repris quand le solde s'est épuisé et 1440 frames sont parties avec
        lui. Un run qui ne laisse rien n'a pas de raison d'exister."""
        source = pathlib.Path(launch_job.__file__).read_text()
        # Sur la ligne où il est, pas le fichier entier : un assertNotIn qui
        # échoue imprime son conteneur, et un lanceur de 440 lignes dans un
        # message d'erreur cache le message.
        offending = [line.strip() for line in source.splitlines()
                     if '"--no-archive"' in line or "args.no_archive" in line]
        self.assertEqual(offending, [], "l'archive ne se désactive pas")

    def test_the_credentials_are_found_without_exporting_anything(self):
        """Un lancement qui exige qu'on pense à une variable est un lancement
        qu'on finit par faire sans elle."""
        found = launch_job.default_pulumi_dir()
        # Le checkout voisin n'est pas garanti sur toute machine ; ce qui est
        # garanti, c'est qu'on le cherche au bon endroit et qu'on répond une
        # chaîne, jamais une exception.
        self.assertIsInstance(found, str)
        if found:
            self.assertTrue(found.endswith("sportstracklive-rails/infra"))


class CardsTheImageCanRun(unittest.TestCase):
    """Ce que l'image sait faire tourner, et sur quelle carte.

    Deux backends, deux règles, et les confondre coûte un pod :
    - OptiX charge du PTX, que le pilote compile à la volée — n'importe quelle
      carte assez récente, 4090 comprise ;
    - CUDA charge `kernel_sm_120.cubin` — Blackwell et rien d'autre.

    Donc la liste par défaut peut contenir de l'Ada, mais elle DOIT contenir
    au moins une Blackwell : sinon le seul chemin restant est OptiX, et si
    OptiX se dérobe il n'y a pas de repli."""

    BLACKWELL = ("5080", "5090", "PRO 5000", "PRO 6000", "B200", "B300")

    def _default_line(self):
        source = pathlib.Path(launch_job.__file__).read_text()
        start = source.index("args.gpu_type = [")
        return source[start:source.index("]", start)]

    def test_at_least_one_default_card_can_load_the_cuda_kernel(self):
        line = self._default_line()
        self.assertTrue(
            any(b in line for b in self.BLACKWELL),
            "aucune carte Blackwell par défaut : sans elle il ne reste que "
            "OptiX, et aucun repli s'il se dérobe")

    def test_the_defaults_are_ordered_cheapest_first(self):
        """La liste est essayée dans l'ordre avant d'attendre ; l'ordre EST le
        choix économique."""
        line = self._default_line()
        order = [line.index(n) for n in ("4090", "5080", "5090") if n in line]
        self.assertEqual(order, sorted(order),
                         "les cartes ne sont plus du moins cher au plus cher")


class SeeingWhatIsRunning(unittest.TestCase):
    """« Aucun pod » doit vouloir dire aucun pod."""

    def test_an_unknown_response_shape_is_an_error_not_an_empty_list(self):
        """Six pods ont tourné pendant que le compte annonçait zéro : l'API
        rend `{"pods": [...]}` et le parseur cherchait `items`, se rabattant
        sur `[]`. Une lecture ratée avait exactement la même tête qu'un
        compte à zéro — et coûtait 0,69 $/h chacun, invisibles."""
        seen = {}
        launch_job.call = lambda key, method, path, body=None: seen.setdefault(
            "r", {"pods": [{"id": "a"}]})
        self.assertEqual(launch_job.all_pods("k"), [{"id": "a"}])

        launch_job.call = lambda key, method, path, body=None: {"items": [1, 2]}
        self.assertEqual(launch_job.all_pods("k"), [1, 2])

        # Et la forme inconnue : refus de conclure.
        launch_job.call = lambda key, method, path, body=None: {"surprise": []}
        with self.assertRaises(SystemExit):
            launch_job.all_pods("k")


class EmptyBodies(unittest.TestCase):
    """Un 204 sans corps n'est pas une erreur."""

    def test_a_delete_that_returns_nothing_is_a_success(self):
        """`--kill-all` a supprimé le pod puis a planté en essayant de parser
        un corps vide. Le verbe qui doit marcher quand tout le reste est
        cassé ne peut pas être le premier à tomber."""
        source = pathlib.Path(launch_job.__file__).read_text()
        self.assertIn("if body.strip() else {}", source,
                      "call() reparse un corps vide en JSON")


class NoPythonInTheImage(unittest.TestCase):
    """Le job ne doit pas appeler python3 : l'image n'en a pas.

    Blender embarque le sien et rien d'autre n'est installé. Deux contrôles
    successifs ont appelé `python3`, n'ont rien produit, et ont quand même
    fait échouer le pod — pour la bonne conclusion et la mauvaise raison.
    C'est le genre de chance qui cesse exactement quand elle compte."""

    def test_the_job_never_calls_a_python_that_is_not_there(self):
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        offending = [l.strip() for l in script.splitlines()
                     if "python3" in l and not l.strip().startswith("#")]
        self.assertEqual(offending, [],
                         "l'image n'a pas de python3 ; utilise `blender "
                         "--python-expr`, ou pas d'interpréteur du tout")


class HostLottery(unittest.TestCase):
    """Un hôte où cuInit échoue n'est pas une impasse, c'est un tirage."""

    def test_the_markers_match_what_the_job_actually_prints(self):
        """Un marqueur qui a dérivé, c'est un retry qui ne se déclenche
        jamais — et sept pods dépensés à la main pour s'en apercevoir."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        for marker in launch_job.BAD_HOST:
            self.assertTrue(marker in script,
                            f"le job n'imprime jamais {marker}")
        # Au moins un signe de démarrage réel doit exister aussi, sinon on
        # supprimerait un pod parfaitement sain au bout de la patience.
        self.assertTrue(any(m.strip() in script for m in launch_job.STARTED),
                        "aucun marqueur de démarrage n'est produit par le job")

    def test_a_bad_host_is_deleted_and_not_left_billing(self):
        """Le bail dort avant de sortir. Un pod écarté qu'on ne supprime pas
        facture pendant qu'il dort."""
        source = pathlib.Path(launch_job.__file__).read_text()
        self.assertIn('call(key, "DELETE", f"/pods/{pod[\'id\']}")', source)


class DepositAsYouGo(unittest.TestCase):
    """Un pod repris en cours de route doit avoir déjà déposé ce qu'il a fait."""

    def test_one_url_per_segment_so_nothing_waits_for_the_concat(self):
        """Attendre le montage, c'est ce qui a fait perdre 1440 frames déjà
        rendues : le pod a été repris et le téléversement n'avait pas eu lieu."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("JOB_SEG_PUT_URL_", script)
        # Le chemin de RÉUSSITE, pas seulement le mot : `SEG-UP-FAILED`
        # contient « SEG-UP », et un test qui s'en contente passe même si le
        # téléversement a été retiré.
        self.assertIn('curl -fsS -T "$outdir/seg$i.mp4" "$url"', script)

    def test_the_logs_go_up_while_it_runs_not_only_at_the_end(self):
        """Le `trap` couvre toutes les façons dont ce script décide de
        s'arrêter. Il ne couvre pas celle dont un pod meurt vraiment :
        SIGKILL, pas de trap, rien d'écrit."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("flush_logs", script)
        self.assertIn("JOB_ARCHIVE_EVERY", script)


class PackedRender(unittest.TestCase):
    """Un rendu nourri par un pack ne doit porter aucun jeton."""

    def test_the_presigned_read_url_is_redacted_like_any_other(self):
        url = ("https://acct.r2.cloudflarestorage.com/bucket/packs/abc/1-48.tuilepack"
               "?X-Amz-Signature=cafebabe&X-Amz-Expires=86400")
        out = launch_job.redacted({"JOB_PACK_URL": url})
        self.assertIn("packs/abc/1-48.tuilepack", out["JOB_PACK_URL"],
                      "on doit pouvoir dire quel pack un run a lu")
        self.assertNotIn("cafebabe", out["JOB_PACK_URL"])

    def test_the_job_script_drops_the_token_once_a_pack_is_in_hand(self):
        """Un jeton posé à côté d'un pack est un jeton qu'on peut encore
        atteindre. Le retirer est ce qui transforme une intention en fait."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("unset TUILE_ION_TOKEN", script)


class FourGpus(unittest.TestCase):
    """Storm ne peut pas utiliser quatre GPU, et le job doit le savoir."""

    def test_the_job_probes_optix_devices_for_the_cycles_path(self):
        """Compter les cartes que le pilote voit n'est pas la même question
        que « sur combien le moteur sait poser du travail ». Répondre à la
        question facile est ce qui a laissé seize processus se partager un
        seul GPU pendant que la sonde en annonçait quatre."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        # Sur des présences, pas sur le fichier entier : un assertIn qui échoue
        # imprime son conteneur, et 400 lignes de bash dans un message d'erreur
        # cachent le message.
        for needle in (
            # Le comptage de cartes ne survit que pour Storm, qui dessine en GL.
            '[ "$JOB_DELEGATE" = "storm" ]',
            # On demande à Cycles, on ne lit pas une énumération : dans cette
            # image `enum_items` répond [] pendant que CUDA est bien là.
            "prefs.compute_device_type = backend",
            "except TypeError",
        ):
            self.assertTrue(needle in script, f"absent du job : {needle}")

    def test_the_delegate_is_told_which_device_or_it_renders_on_the_cpu(self):
        """hdCycles lit son device dans CYCLES_DEVICE et retombe en silence
        sur le CPU si personne ne parle. Quatre RTX 5090 inertes pendant que
        seize processus font du path tracing sur l'hôte, ça ressemble
        exactement à un job lent."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        self.assertTrue('CYCLES_DEVICE="$CYCLES_BACKEND"' in script,
                        "le délégué n'est pas informé de son device")

    def test_the_job_reports_whether_the_gpus_actually_worked(self):
        """Un run qui n'a utilisé qu'un GPU sur quatre n'est pas un run lent,
        c'est un run cassé — et ça ne doit pas demander qu'un humain regarde
        une capture d'écran."""
        script = (pathlib.Path(launch_job.__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("GPU-UNDERUSED", script)


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
