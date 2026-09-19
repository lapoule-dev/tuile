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
import re
import subprocess
import sys
import types
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


class TasksSplitTheRange(unittest.TestCase):
    """La tranche d'une tâche, exécutée et non relue.

    Un test qui cherche `task_index * base` dans le texte du script prouve
    qu'une chaîne y figure. Ce qui compte est ailleurs : que les N tranches
    recouvrent exactement la plage demandée, sans trou ni recouvrement. Un trou
    est une vidéo plus courte que ce qu'on a payé, un recouvrement est une
    frame rendue deux fois et montée deux fois. Alors on extrait le bloc du
    vrai script et on le fait tourner sous bash.
    """

    JOB = pathlib.Path(__file__).with_name("render_job.sh")

    def _slice(self, task_index, task_count, first, last):
        """Ce que render_job.sh calcule pour une tâche, réellement exécuté."""
        text = self.JOB.read_text()
        start = text.index('if [ "$task_count" -gt 1 ]; then')
        end = text.index("\nfi\n", start) + len("\nfi\n")
        block = text[start:end]
        self.assertIn("task_index * base", block,
                      "le bloc extrait n'est pas celui de la découpe")
        script = (
            "task_index=$1; task_count=$2; first=$3; last=$4\n"
            + block
            + 'printf "%s %s\\n" "$first" "$last"\n'
        )
        out = subprocess.run(
            ["bash", "-c", script, "bash",
             str(task_index), str(task_count), str(first), str(last)],
            capture_output=True, text=True, check=True)
        a, b = out.stdout.strip().splitlines()[-1].split()
        return int(a), int(b)

    def test_the_slices_cover_the_range_exactly_and_never_overlap(self):
        for first, last in ((1, 48), (1, 1440), (100, 107), (7, 7 + 96)):
            for n in (1, 2, 3, 5, 7):
                covered = []
                for t in range(n):
                    a, b = self._slice(t, n, first, last)
                    self.assertLessEqual(a, b,
                                         f"tâche {t}/{n} sur {first}:{last} "
                                         "a une tranche vide")
                    covered.extend(range(a, b + 1))
                self.assertEqual(
                    covered, list(range(first, last + 1)),
                    f"{n} tâches sur {first}:{last} ne recouvrent pas la plage")

    def test_the_remainder_is_spread_not_dumped_on_the_last_task(self):
        # 48 sur 5 : 10,10,10,9,9 — et non 9,9,9,9,12. L'écart maximal entre
        # deux tranches est d'une frame, donc la tâche la plus lente, qui fixe
        # le temps de mur et la facture, ne porte pas tout le reste.
        sizes = [b - a + 1 for a, b in
                 (self._slice(t, 5, 1, 48) for t in range(5))]
        self.assertEqual(sizes, [10, 10, 10, 9, 9])
        self.assertLessEqual(max(sizes) - min(sizes), 1)

    def test_a_lone_container_renders_the_whole_range(self):
        # Sans CLOUD_RUN_TASK_COUNT, un pod lit 0 sur 1 et se comporte comme
        # avant que les tâches existent.
        text = self.JOB.read_text()
        self.assertIn('task_count="${CLOUD_RUN_TASK_COUNT:-1}"', text)
        self.assertIn('task_index="${CLOUD_RUN_TASK_INDEX:-0}"', text)
        self.assertEqual(self._slice(0, 1, 1, 48), (1, 48))

    def test_segments_are_numbered_globally_so_assembly_can_order_them(self):
        # Deux tâches de 4 processus produisent les segments 0-3 et 4-7, pas
        # 0-3 deux fois : sinon la seconde écrase les dépôts de la première.
        text = self.JOB.read_text()
        self.assertIn("seg_base=$((task_index * jobs))", text)
        self.assertIn('eval "url=\\${JOB_SEG_PUT_URL_$g:-}"', text)

    def test_each_task_ships_its_own_logs(self):
        # Une seule JOB_LOGS_PUT_URL pour N tâches, et le survivant est celui
        # qui a fini en dernier — jamais celui qui a échoué.
        text = self.JOB.read_text()
        self.assertIn("for var in JOB_LOGS_PUT_URL JOB_TRACE_PUT_URL "
                      "JOB_PROFILE_PUT_URL; do", text)

    def test_only_a_lone_task_concatenates_and_uploads_the_film(self):
        text = self.JOB.read_text()
        concat = text.index("ffmpeg -y -f concat")
        guard = text.rindex('if [ "$task_count" -gt 1 ]; then', 0, concat)
        self.assertIn("TASK-DONE", text[guard:concat],
                      "le montage n'est pas gardé par le nombre de tâches")


class TheGoogleSide(unittest.TestCase):
    """Ce qu'on envoie à Cloud Run, sans réseau.

    Chaque erreur de forme ici se paie en secondes de L4 et en aller-retours
    d'une minute. `validateOnly` existe pour ça, mais encore faut-il que le
    corps soit juste avant de le valider.
    """

    def _capture(self, fn, reply=None):
        """Appelle `fn` en interceptant gcp_call, et rend ce qui est parti."""
        sent = []

        def fake(method, path, body=None, token=None):
            sent.append((method, path, body))
            return reply if reply is not None else {}

        original = launch_job.gcp_call
        launch_job.gcp_call = fake
        try:
            result = fn()
        finally:
            launch_job.gcp_call = original
        return sent, result

    def test_the_env_travels_as_name_value_pairs_not_a_comma_list(self):
        # La raison de parler REST plutôt que `gcloud run jobs execute` :
        # --update-env-vars sépare par des virgules, et une URL présignée qui
        # en contiendrait une couperait le lancement en deux variables.
        env = {"JOB_FRAMES": "1:48", "JOB_PACK_URL": "https://x/y?a=1,b=2"}
        sent, name = self._capture(
            lambda: launch_job.gcp_run(env, 3, token="t"),
            reply={"metadata": {"name": "projects/p/locations/l/jobs/j/"
                                        "executions/tuile-render-abcde"}})
        (method, path, body), = sent
        self.assertEqual(method, "POST")
        self.assertTrue(path.endswith(":run"), path)
        pairs = body["overrides"]["containerOverrides"][0]["env"]
        self.assertEqual(
            sorted(pairs, key=lambda d: d["name"]),
            [{"name": "JOB_FRAMES", "value": "1:48"},
             {"name": "JOB_PACK_URL", "value": "https://x/y?a=1,b=2"}])
        self.assertEqual(body["overrides"]["taskCount"], 3)
        self.assertNotIn("validateOnly", body)
        self.assertEqual(name.rsplit("/", 1)[-1], "tuile-render-abcde")

    def test_a_dry_run_asks_for_validation_and_returns_nothing(self):
        sent, name = self._capture(
            lambda: launch_job.gcp_run({"A": "1"}, 1, dry_run=True, token="t"))
        self.assertTrue(sent[0][2]["validateOnly"])
        self.assertIsNone(name, "un dry-run ne doit pas prétendre à une "
                                "exécution qui n'existe pas")

    def test_an_answer_we_cannot_read_is_not_an_empty_list(self):
        # La panne du 10 septembre, transposée : `d.get('items', [])` a rendu
        # zéro pendant que six pods facturaient. Une forme inconnue est une
        # erreur, jamais une absence.
        original = launch_job.gcp_call
        launch_job.gcp_call = lambda *a, **k: {"jobExecutions": [{"name": "x"}]}
        try:
            with self.assertRaises(SystemExit):
                launch_job.gcp_executions(token="t")
        finally:
            launch_job.gcp_call = original

    def test_an_empty_body_really_is_no_executions(self):
        original = launch_job.gcp_call
        launch_job.gcp_call = lambda *a, **k: {}
        try:
            self.assertEqual(launch_job.gcp_executions(token="t"), [])
        finally:
            launch_job.gcp_call = original

    def test_an_execution_with_no_completion_time_is_still_billing(self):
        running = {"name": "p/l/j/executions/e1", "taskCount": 3,
                   "runningCount": 2, "succeededCount": 1,
                   "createTime": "2026-09-15T10:00:00Z"}
        done = dict(running, name="p/l/j/executions/e2",
                    completionTime="2026-09-15T10:30:00Z")
        self.assertIn("en cours", launch_job.gcp_execution_line(running))
        self.assertIn("terminée", launch_job.gcp_execution_line(done))

    def test_the_image_comes_from_the_job_not_from_a_string_we_built(self):
        original = launch_job.gcp_call
        launch_job.gcp_call = lambda *a, **k: {
            "template": {"template": {"containers": [{"image": "gar/x:tag"}]}}}
        try:
            self.assertEqual(launch_job.gcp_image_of_job(token="t"), "gar/x:tag")
        finally:
            launch_job.gcp_call = original

    def test_a_job_without_an_image_says_so_instead_of_launching(self):
        original = launch_job.gcp_call
        launch_job.gcp_call = lambda *a, **k: {"template": {"template": {}}}
        try:
            with self.assertRaises(SystemExit) as caught:
                launch_job.gcp_image_of_job(token="t")
        finally:
            launch_job.gcp_call = original
        self.assertIn("pulumi up", str(caught.exception))

    def test_the_backend_is_readable_before_argparse_runs(self):
        # --list et --kill-all sont traités avant le parseur : ils doivent
        # marcher quand le lancement est cassé, et savoir quand même à qui
        # parler.
        self.assertEqual(launch_job.backend_of(["--list"]), "gcp")
        self.assertEqual(
            launch_job.backend_of(["--list", "--backend", "runpod"]), "runpod")
        self.assertEqual(
            launch_job.backend_of(["--kill-all", "--backend=runpod"]), "runpod")


class OneArchivePerTask(unittest.TestCase):
    """N tâches, N jeux de journaux.

    Une seule clé partagée et le survivant est celui qui a fini en dernier —
    jamais celui qui a échoué, qui est pourtant le seul qu'on voulait lire.
    """

    def _urls(self, segments, tasks):
        made = []

        class FakeClient:
            def generate_presigned_url(self, op, Params, ExpiresIn):
                made.append(Params["Key"])
                return "https://r2/" + Params["Key"]

        original = launch_job.r2_credentials
        launch_job.r2_credentials = lambda: ("k", "s")
        import boto3
        original_client = boto3.client
        boto3.client = lambda *a, **k: FakeClient()
        try:
            _, urls = launch_job.archive_urls("run-1", segments, tasks)
        finally:
            launch_job.r2_credentials = original
            boto3.client = original_client
        return urls

    def test_a_lone_task_needs_no_suffixed_archive(self):
        urls = self._urls(4, 1)
        self.assertIn("logs.tar.gz", urls)
        self.assertNotIn("logs-t0", urls)

    def test_every_task_gets_its_own_logs_trace_and_profile(self):
        urls = self._urls(12, 3)
        for t in range(3):
            for kind in ("logs", "trace", "profile"):
                self.assertIn(f"{kind}-t{t}", urls,
                              f"la tâche {t} n'a pas d'archive {kind}")
        self.assertNotIn("logs-t3", urls)

    def test_there_is_one_url_per_segment_across_every_task(self):
        # Trois tâches de quatre processus font douze segments, numérotés 0..11
        # d'un bout à l'autre — et non 0..3 trois fois, qui se recouvriraient.
        urls = self._urls(12, 3)
        self.assertEqual([f"seg{i}" for i in range(12)],
                         [k for k in urls if k.startswith("seg")])


class HowManySegments(unittest.TestCase):
    """Le compte des segments, qui fixe le compte des URL présignées.

    Une URL de trop ne coûte rien. Une de moins, et le dernier processus rend
    un segment qu'il ne peut déposer nulle part — ce qui ne se voit qu'au
    montage, quand le film est plus court que demandé.
    """

    def test_gcp_counts_tasks_because_a_task_owns_exactly_one_gpu(self):
        self.assertEqual(launch_job.segment_count("gcp", 3, 1, 4), 12)
        self.assertEqual(launch_job.segment_count("gcp", 1, 1, 4), 4)

    def test_runpod_counts_gpus_because_a_pod_owns_several(self):
        self.assertEqual(launch_job.segment_count("runpod", 1, 4, 4), 16)

    def test_the_task_count_is_ignored_on_a_pod(self):
        # --tasks est un drapeau gcp. S'il comptait aussi côté runpod, un
        # oubli donnerait quatre fois trop d'URL et aucun symptôme.
        self.assertEqual(launch_job.segment_count("runpod", 7, 2, 3),
                         launch_job.segment_count("runpod", 1, 2, 3))


class TheImagePathIsOneString(unittest.TestCase):
    """Le chemin poussé et le chemin déclaré doivent être le même.

    Deux fichiers, deux dépôts, une seule chaîne — et Cloud Run **valide
    l'existence de l'image au moment de créer le job**, pas au lancement. Un
    caractère d'écart et la création répond `Error code 5: Image … not found`,
    ce qui se lit comme un problème de droits.

    S'abstient quand le checkout d'infra n'est pas là : ce test dit quelque
    chose quand il peut, et rien quand il ne peut pas — jamais une réussite
    qu'il n'a pas vérifiée.
    """

    FARM = (pathlib.Path(__file__).resolve().parents[3]
            / "sportstracklive-rails" / "infra" / "render_farm.py")

    def setUp(self):
        if not self.FARM.is_file():
            self.skipTest(f"{self.FARM} absent — projet Pulumi non présent")
        self.farm = self.FARM.read_text()
        self.push = (pathlib.Path(__file__).with_name("build-push.sh")
                     .read_text())

    def test_the_registry_host_and_repository_agree(self):
        for needle in ("-docker.pkg.dev", "/tuile/"):
            self.assertIn(needle, self.farm)
        self.assertIn('GAR_HOST="${GCP_REGION}-docker.pkg.dev"', self.push)
        self.assertIn('GCP_REPO="${TUILE_GCP_REPO:-tuile}"', self.push)

    def test_the_region_is_the_same_on_both_sides(self):
        # La région n'est plus une constante : elle est dérivée de la carte,
        # parce que chaque accélérateur n'existe que dans certaines régions et
        # qu'un couple invalide est refusé à la création du job. Ce qui doit
        # rester vrai, c'est que la région par défaut de la carte par défaut
        # soit celle où le script pousse l'image — sinon chaque démarrage à
        # froid traverse une frontière, facturé en egress.
        self.assertIn('_accelerator = _config.get("accelerator") or "nvidia-l4"',
                      self.farm, "la carte par défaut a changé")
        l4 = self.farm[self.farm.index('"nvidia-l4": {'):]
        l4 = l4[:l4.index("},")]
        self.assertIn('"europe-west1"', l4,
                      "europe-west1 n'est plus la première région du L4")
        self.assertIn('GCP_REGION="${TUILE_GCP_REGION:-europe-west1}"',
                      self.push)

    def test_the_tag_is_the_same_on_both_sides(self):
        # Le tag vit dans le job Pulumi ET dans la cible `globe` du script.
        # Quand ils divergent, le job tire une image que personne n'a poussée.
        self.assertIn('_image_tag = _config.get("image_tag") or "5.1-su"',
                      self.farm)
        globe = self.push[self.push.index("    globe)"):]
        self.assertIn("TAG=5.1-su", globe[:globe.index(";;")])

    def test_the_stl_prefix_is_dropped_under_artifact_registry(self):
        # `stl/` est un espace de noms chez ECR et Harbor ; chez Google c'est
        # le dépôt qui l'est, et un `stl/` de trop donne un chemin à quatre
        # segments que le job ne trouvera jamais.
        self.assertIn('${REPO#*/}', self.push)
        self.assertIn("/tuile/blender-globe:", self.farm)


class ArchivingWhileItWrites(unittest.TestCase):
    """Le flush périodique doit survivre à un fichier qui bouge.

    Il existe pour ça et pour rien d'autre : téléverser les journaux PENDANT
    le rendu, parce qu'une tâche tuée n'exécute aucun piège. `tar` rend 1 —
    pas 2 — quand un fichier a changé pendant la lecture, et l'archive produite
    est complète. Traiter ce 1 comme fatal rendait le flush inopérant à chaque
    tour. Mesuré le 15 septembre : vingt-sept minutes de rendu Cloud Run, zéro
    archive, diagnostic perdu avec la tâche.

    Ce test exécute vraiment `ship`, avec un fichier qu'un autre processus
    allonge pendant l'archivage.
    """

    JOB = pathlib.Path(__file__).with_name("render_job.sh")

    def _ship(self, grow):
        """Extrait `ship` du vrai script et l'exécute. Rend son stdout."""
        text = self.JOB.read_text()
        start = text.index("ship() {")
        end = text.index("\n}\n", start) + len("\n}\n")
        body = text[start:end]
        self.assertIn("tar czf", body)
        import tempfile as _tempfile
        with _tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            (out / "log-s0.txt").write_text("une ligne\n" * 100)
            (out / "job.log").write_text("x" * 100_000)
            grower = ""
            if grow:
                # Un écrivain qui allonge job.log pendant que tar le lit :
                # c'est `tee` dans le vrai script.
                grower = ('( for k in $(seq 1 400); do '
                          'printf "%s" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" '
                          '>> "$outdir/job.log"; done ) & ')
            script = (
                f'outdir="{tmp}"\n'
                'curl() { echo "CURL $*" >> "$outdir/curl.log"; return 0; }\n'
                + body
                + grower
                + "ship logs.tar.gz https://example/put 'log-s*.txt' 'job.log'\n"
                "wait\n")
            done = subprocess.run(["bash", "-c", script],
                                  capture_output=True, text=True)
            return done.stdout, (out / "logs.tar.gz").exists()

    def test_a_file_growing_under_tar_still_ships(self):
        stdout, made = self._ship(grow=True)
        self.assertTrue(made, "aucune archive produite")
        self.assertIn("ARCHIVE-UP logs.tar.gz", stdout,
                      f"rien n'a été téléversé — sortie: {stdout!r}")
        self.assertNotIn("ARCHIVE-TAR-FAILED", stdout)

    def test_a_quiet_directory_ships_too(self):
        stdout, made = self._ship(grow=False)
        self.assertTrue(made)
        self.assertIn("ARCHIVE-UP logs.tar.gz", stdout)

    def test_a_failure_is_no_longer_swallowed_by_dev_null(self):
        # Le flush envoyait tout dans /dev/null : la panne était muette.
        text = self.JOB.read_text()
        flush = text[text.index("flush_logs() {"):]
        flush = flush[:flush.index("\n}\n")]
        self.assertNotIn("> /dev/null 2>&1", flush)
        self.assertIn("*FAILED*", flush)


class DyingRendersMustNotBillAnHour(unittest.TestCase):
    """Un rendu mort doit rendre la main tout de suite.

    `wait` sans argument attend TOUS les enfants du shell, et le job en garde
    deux qui ne finissent jamais : l'échantillonneur GPU et le flush de
    journaux, tous deux en boucle. La ligne était donc infranchissable même
    quand chaque rendu était mort.

    Mesuré le 16 septembre : Blender a crashé après dix secondes
    (`the frame did not converge`), et la tâche a continué d'échantillonner un
    GPU inactif jusqu'au délai d'une heure. Elle facturait un L4 tout du long,
    et de l'extérieur — `GPU-USE` toutes les trente secondes — elle avait
    l'air de travailler. C'est ce qu'on a pris pour un calcul lent pendant
    deux jours.
    """

    def test_a_crashed_render_does_not_hold_the_script_open(self):
        # La vraie forme du script : deux boucles sans fin, des « rendus » qui
        # meurent, et un wait. Sans les PID, ce script ne se termine jamais.
        script = """
        sampler() { while true; do sleep 0.05; done; }
        flusher() { while true; do sleep 0.05; done; }
        sampler & gpu_watch=$!
        flusher & log_flusher=$!
        renders=()
        for i in 0 1; do
            ( exit 134 ) | cat | cat &
            renders+=($!)
        done
        wait "${renders[@]}"
        kill "$gpu_watch" "$log_flusher" 2>/dev/null || true
        echo LIBRE
        """
        done = subprocess.run(["bash", "-c", script], capture_output=True,
                              text=True, timeout=15)
        self.assertIn("LIBRE", done.stdout,
                      "le script n'a pas repris la main après la mort des rendus")

    def test_the_job_waits_on_the_render_pids_and_not_on_everything(self):
        text = (pathlib.Path(__file__).with_name("render_job.sh")).read_text()
        self.assertIn('wait "${renders[@]}"', text)
        self.assertNotIn("\nwait\n", text,
                         "un `wait` nu subsiste : il attendrait aussi les "
                         "boucles infinies")
        self.assertIn("renders+=($!)", text)


class ThePackNameComesFromTheParameters(unittest.TestCase):
    """Le nom du pack se calcule avant que quoi que ce soit démarre.

    La version d'avant nommait l'objet d'après le digest de scène, que seul le
    job calcule — il couvre les réglages de traversée résolus. Le pack partait
    donc sous une clef neutre, le lanceur lisait `BAKE-KEY` dans les journaux,
    puis copiait. Deux des trois étapes vivaient dans le processus du lanceur.

    Mesuré le 16 septembre 2026 : le lanceur s'est arrêté entre la cuisson et
    la copie, et deux gigaoctets parfaitement valides sont restés sous une clef
    que personne ne cherche. Un rangement qui exige qu'une fenêtre de terminal
    reste ouverte n'est pas un rangement.
    """

    def _args(self, **over):
        base = dict(trajectory="orbit:1440:2.17:42.52:8000:5000",
                    frames="1:1440", viewport="1280x960", sse=3.0,
                    imagery_boost=1, imagery=0, terrain=0)
        base.update(over)
        return types.SimpleNamespace(**base)

    def test_the_same_parameters_always_give_the_same_key(self):
        a = launch_job.pack_key(self._args())
        b = launch_job.pack_key(self._args())
        self.assertEqual(a, b)
        self.assertTrue(a.startswith("packs/"), a)
        self.assertTrue(a.endswith("/1-1440.tuilepack"), a)

    def test_every_baking_parameter_changes_the_key(self):
        base = launch_job.pack_key(self._args())
        for field, value in (("trajectory", "orbit:1440:2.18:42.52:8000:5000"),
                             ("frames", "1:720"),
                             ("viewport", "1920x1080"),
                             ("sse", 2.0),
                             # De combien de niveaux l'imagerie descend sous le
                             # terrain : deux packs qui n'en ont pas la même
                             # valeur ne portent pas la même imagerie.
                             ("imagery_boost", 3),
                             # Les sources : Sentinel-2 n'est pas Bing Aerial,
                             # et un pack de l'un ne peut pas servir pour
                             # l'autre.
                             ("imagery", 3954),
                             ("terrain", 2767062)):
            self.assertNotEqual(
                base, launch_job.pack_key(self._args(**{field: value})),
                f"changer {field} ne change pas la clef : deux cuissons "
                "différentes s'écraseraient l'une l'autre")

    def test_the_range_is_in_the_name_not_in_the_hash(self):
        # Deux tranches d'un même plan partagent le répertoire et se
        # distinguent par le fichier — c'est ce qui permettra de cuire en
        # parallèle sans que les morceaux se perdent.
        whole = launch_job.pack_key(self._args())
        half = launch_job.pack_key(self._args(frames="1:720"))
        self.assertNotEqual(whole.rsplit("/", 1)[0], half.rsplit("/", 1)[0])
        self.assertEqual(half.rsplit("/", 1)[1], "1-720.tuilepack")

    def test_nothing_is_left_for_the_launcher_to_do_afterwards(self):
        source = (pathlib.Path(__file__).with_name("launch_job.py")).read_text()
        # Depuis `bake_env`, pas depuis `bake` : l'environnement du job est
        # sorti en fonction pure pour être testable sans lancer de cuisson, et
        # les clefs de dépôt vivent désormais là. La tranche doit couvrir les
        # deux, sinon elle affirme une absence qui n'est qu'un déménagement.
        bake = source[source.index("def bake_env("):source.index("def assemble(")]
        self.assertNotIn("copy_object", bake,
                         "le lanceur range encore le pack après coup")
        self.assertNotIn("bake_key_of", bake)
        self.assertIn("JOB_PACK_PUT_URL", bake)
        self.assertIn("JOB_SCENE_PUT_URL", bake,
                      "le digest de scène n'est plus déposé : un rendu ne "
                      "pourrait plus vérifier qu'on lui donne le bon globe")


class HowManySegments(unittest.TestCase):
    """Le compte des segments, qui fixe le compte des URL présignées.

    Une URL de trop ne coûte rien. Une de moins, et le dernier processus rend
    un segment qu'il ne peut déposer nulle part — ce qui ne se voit qu'au
    montage, quand le film est plus court que demandé.
    """

    def test_gcp_counts_tasks_because_a_task_owns_exactly_one_gpu(self):
        self.assertEqual(launch_job.segment_count("gcp", 3, 1, 4), 12)
        self.assertEqual(launch_job.segment_count("gcp", 1, 1, 4), 4)

    def test_runpod_counts_gpus_because_a_pod_owns_several(self):
        self.assertEqual(launch_job.segment_count("runpod", 1, 4, 4), 16)

    def test_the_task_count_is_ignored_on_a_pod(self):
        # --tasks est un drapeau gcp. S'il comptait aussi côté runpod, un
        # oubli donnerait quatre fois trop d'URL et aucun symptôme.
        self.assertEqual(launch_job.segment_count("runpod", 7, 2, 3),
                         launch_job.segment_count("runpod", 1, 2, 3))


class TheImagePathIsOneString(unittest.TestCase):
    """Le chemin poussé et le chemin déclaré doivent être le même.

    Deux fichiers, deux dépôts, une seule chaîne — et Cloud Run **valide
    l'existence de l'image au moment de créer le job**, pas au lancement. Un
    caractère d'écart et la création répond `Error code 5: Image … not found`,
    ce qui se lit comme un problème de droits.

    S'abstient quand le checkout d'infra n'est pas là : ce test dit quelque
    chose quand il peut, et rien quand il ne peut pas — jamais une réussite
    qu'il n'a pas vérifiée.
    """

    FARM = (pathlib.Path(__file__).resolve().parents[3]
            / "sportstracklive-rails" / "infra" / "render_farm.py")

    def setUp(self):
        if not self.FARM.is_file():
            self.skipTest(f"{self.FARM} absent — projet Pulumi non présent")
        self.farm = self.FARM.read_text()
        self.push = (pathlib.Path(__file__).with_name("build-push.sh")
                     .read_text())

    def test_the_registry_host_and_repository_agree(self):
        for needle in ("-docker.pkg.dev", "/tuile/"):
            self.assertIn(needle, self.farm)
        self.assertIn('GAR_HOST="${GCP_REGION}-docker.pkg.dev"', self.push)
        self.assertIn('GCP_REPO="${TUILE_GCP_REPO:-tuile}"', self.push)

    def test_the_region_is_the_same_on_both_sides(self):
        # La région n'est plus une constante : elle est dérivée de la carte,
        # parce que chaque accélérateur n'existe que dans certaines régions et
        # qu'un couple invalide est refusé à la création du job. Ce qui doit
        # rester vrai, c'est que la région par défaut de la carte par défaut
        # soit celle où le script pousse l'image — sinon chaque démarrage à
        # froid traverse une frontière, facturé en egress.
        self.assertIn('_accelerator = _config.get("accelerator") or "nvidia-l4"',
                      self.farm, "la carte par défaut a changé")
        l4 = self.farm[self.farm.index('"nvidia-l4": {'):]
        l4 = l4[:l4.index("},")]
        self.assertIn('"europe-west1"', l4,
                      "europe-west1 n'est plus la première région du L4")
        self.assertIn('GCP_REGION="${TUILE_GCP_REGION:-europe-west1}"',
                      self.push)

    def test_the_tag_is_the_same_on_both_sides(self):
        # Le tag vit dans le job Pulumi ET dans la cible `globe` du script.
        # Quand ils divergent, le job tire une image que personne n'a poussée.
        self.assertIn('_image_tag = _config.get("image_tag") or "5.1-su"',
                      self.farm)
        globe = self.push[self.push.index("    globe)"):]
        self.assertIn("TAG=5.1-su", globe[:globe.index(";;")])

    def test_the_stl_prefix_is_dropped_under_artifact_registry(self):
        # `stl/` est un espace de noms chez ECR et Harbor ; chez Google c'est
        # le dépôt qui l'est, et un `stl/` de trop donne un chemin à quatre
        # segments que le job ne trouvera jamais.
        self.assertIn('${REPO#*/}', self.push)
        self.assertIn("/tuile/blender-globe:", self.farm)


class ArchivingWhileItWrites(unittest.TestCase):
    """Le flush périodique doit survivre à un fichier qui bouge.

    Il existe pour ça et pour rien d'autre : téléverser les journaux PENDANT
    le rendu, parce qu'une tâche tuée n'exécute aucun piège. `tar` rend 1 —
    pas 2 — quand un fichier a changé pendant la lecture, et l'archive produite
    est complète. Traiter ce 1 comme fatal rendait le flush inopérant à chaque
    tour. Mesuré le 15 septembre : vingt-sept minutes de rendu Cloud Run, zéro
    archive, diagnostic perdu avec la tâche.

    Ce test exécute vraiment `ship`, avec un fichier qu'un autre processus
    allonge pendant l'archivage.
    """

    JOB = pathlib.Path(__file__).with_name("render_job.sh")

    def _ship(self, grow):
        """Extrait `ship` du vrai script et l'exécute. Rend son stdout."""
        text = self.JOB.read_text()
        start = text.index("ship() {")
        end = text.index("\n}\n", start) + len("\n}\n")
        body = text[start:end]
        self.assertIn("tar czf", body)
        import tempfile as _tempfile
        with _tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            (out / "log-s0.txt").write_text("une ligne\n" * 100)
            (out / "job.log").write_text("x" * 100_000)
            grower = ""
            if grow:
                # Un écrivain qui allonge job.log pendant que tar le lit :
                # c'est `tee` dans le vrai script.
                grower = ('( for k in $(seq 1 400); do '
                          'printf "%s" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" '
                          '>> "$outdir/job.log"; done ) & ')
            script = (
                f'outdir="{tmp}"\n'
                'curl() { echo "CURL $*" >> "$outdir/curl.log"; return 0; }\n'
                + body
                + grower
                + "ship logs.tar.gz https://example/put 'log-s*.txt' 'job.log'\n"
                "wait\n")
            done = subprocess.run(["bash", "-c", script],
                                  capture_output=True, text=True)
            return done.stdout, (out / "logs.tar.gz").exists()

    def test_a_file_growing_under_tar_still_ships(self):
        stdout, made = self._ship(grow=True)
        self.assertTrue(made, "aucune archive produite")
        self.assertIn("ARCHIVE-UP logs.tar.gz", stdout,
                      f"rien n'a été téléversé — sortie: {stdout!r}")
        self.assertNotIn("ARCHIVE-TAR-FAILED", stdout)

    def test_a_quiet_directory_ships_too(self):
        stdout, made = self._ship(grow=False)
        self.assertTrue(made)
        self.assertIn("ARCHIVE-UP logs.tar.gz", stdout)

    def test_a_failure_is_no_longer_swallowed_by_dev_null(self):
        # Le flush envoyait tout dans /dev/null : la panne était muette.
        text = self.JOB.read_text()
        flush = text[text.index("flush_logs() {"):]
        flush = flush[:flush.index("\n}\n")]
        self.assertNotIn("> /dev/null 2>&1", flush)
        self.assertIn("*FAILED*", flush)


class DyingRendersMustNotBillAnHour(unittest.TestCase):
    """Un rendu mort doit rendre la main tout de suite.

    `wait` sans argument attend TOUS les enfants du shell, et le job en garde
    deux qui ne finissent jamais : l'échantillonneur GPU et le flush de
    journaux, tous deux en boucle. La ligne était donc infranchissable même
    quand chaque rendu était mort.

    Mesuré le 16 septembre : Blender a crashé après dix secondes
    (`the frame did not converge`), et la tâche a continué d'échantillonner un
    GPU inactif jusqu'au délai d'une heure. Elle facturait un L4 tout du long,
    et de l'extérieur — `GPU-USE` toutes les trente secondes — elle avait
    l'air de travailler. C'est ce qu'on a pris pour un calcul lent pendant
    deux jours.
    """

    def test_a_crashed_render_does_not_hold_the_script_open(self):
        # La vraie forme du script : deux boucles sans fin, des « rendus » qui
        # meurent, et un wait. Sans les PID, ce script ne se termine jamais.
        script = """
        sampler() { while true; do sleep 0.05; done; }
        flusher() { while true; do sleep 0.05; done; }
        sampler & gpu_watch=$!
        flusher & log_flusher=$!
        renders=()
        for i in 0 1; do
            ( exit 134 ) | cat | cat &
            renders+=($!)
        done
        wait "${renders[@]}"
        kill "$gpu_watch" "$log_flusher" 2>/dev/null || true
        echo LIBRE
        """
        done = subprocess.run(["bash", "-c", script], capture_output=True,
                              text=True, timeout=15)
        self.assertIn("LIBRE", done.stdout,
                      "le script n'a pas repris la main après la mort des rendus")

    def test_the_job_waits_on_the_render_pids_and_not_on_everything(self):
        text = (pathlib.Path(__file__).with_name("render_job.sh")).read_text()
        self.assertIn('wait "${renders[@]}"', text)
        self.assertNotIn("\nwait\n", text,
                         "un `wait` nu subsiste : il attendrait aussi les "
                         "boucles infinies")
        self.assertIn("renders+=($!)", text)




class TheTwoJobsAgreeOnTheTrajectory(unittest.TestCase):
    """Cuire et rendre doivent fabriquer la MÊME trajectoire.

    Les deux jobs lisent la même chaîne `JOB_TRAJECTORY` et la donnent chacun à
    son générateur, avec ses propres valeurs par défaut écrites en dur. Rien ne
    les tient ensemble : ajouter un genre d'un côté produit un rendu qui meurt
    sur `TRAJECTORY-UNKNOWN`, et — bien pire — changer un défaut d'un seul côté
    produit un pack qui décrit un tournage et une image qui en montre un autre,
    les deux jobs annonçant une réussite.
    """

    @staticmethod
    def _kinds(name):
        """Les genres et leurs défauts, lus dans le `case` du script."""
        text = (pathlib.Path(__file__).with_name(name)).read_text()
        block = text[text.index("IFS=: read -r kind"):]
        block = block[:block.index("esac")]
        kinds = {}
        for line in block.splitlines():
            line = line.strip()
            if ")" not in line or line.startswith("#") or line.startswith("*"):
                continue
            kind = line.split(")")[0].strip()
            if kind and kind.isidentifier():
                kinds[kind] = re.findall(r'\$\{p\d:-([^}]*)\}', block[block.index(line):])
        return kinds

    def test_both_scripts_know_the_same_kinds(self):
        bake = set(self._kinds("bake_job.sh"))
        render = set(self._kinds("render_job.sh"))
        self.assertEqual(bake, render,
                         f"genres divergents — cuisson {sorted(bake)}, "
                         f"rendu {sorted(render)}")

    def test_the_defaults_are_the_same_on_both_sides(self):
        bake = self._kinds("bake_job.sh")
        render = self._kinds("render_job.sh")
        for kind in sorted(set(bake) & set(render)):
            with self.subTest(kind=kind):
                self.assertEqual(
                    bake[kind][:4], render[kind][:4],
                    f"{kind}: la cuisson prend {bake[kind][:4]} et le rendu "
                    f"{render[kind][:4]} — le pack et l'image ne décriraient "
                    "pas le même tournage")


class TheCameraCrossesTheBoundary(unittest.TestCase):
    """La caméra doit atteindre le procédural, et elle ne l'a jamais fait.

    hdGp passe ses arguments par les primvars de la prim, et un primvar est un
    **attribut**. Les deux écrivains de ce projet produisaient pourtant des
    *relationships* préfixés `primvars:` — `tuile-usd` dans le manifeste, et un
    hook d'export dans `render_usd.py` au moment de retargeter sur la caméra
    Blender. USD accepte les deux sans broncher ; le procédural n'en voit
    aucun.

    Conséquence, mesurée le 16 septembre 2026 : `_ResolveCamera` descendait ses
    quatre échelons sans rien trouver et retombait sur la vue fixe au-dessus de
    l'origine de rendu — le centre de l'orbite. Le pack répondait « no baked
    frame answers this camera: the nearest is frame 1108, 8000.000 m away » :
    8000 m est le rayon de l'orbite au millimètre, et 1108 un tirage arbitraire
    parmi 1440 poses toutes équidistantes de ce centre.

    Le hook d'export n'existe plus : le rendu passe par la fast path de
    Blender, qui n'exporte aucun stage. Ce que le procédural lit maintenant est
    `/freeCamera`, la caméra par laquelle l'image est réellement faite, et ce
    barreau est testé en C++ (`integrations/hydra/tests/cameraRungTest.cpp`).
    Ce qui reste ici, c'est le câblage côté pilote : la fast path et le chemin
    du manifeste. Les deux sont des chaînes dans un fichier Python, invisibles
    au compilateur, et chacune casse en silence — la première en faisant
    reconstruire toute la scène à chaque frame, la seconde en rendant sans
    globe.
    """

    def test_the_driver_asks_for_the_fast_path(self):
        source = (pathlib.Path(__file__).with_name("render_usd.py")).read_text()
        code = "\n".join(l for l in source.splitlines()
                         if not l.lstrip().startswith("#"))
        # `assertTrue` sur un booléen plutôt qu'`assertIn` sur le fichier :
        # unittest imprime la valeur comparée, et un `assertIn` raté crachait
        # les dix kilo-octets du pilote au milieu du rapport.
        self.assertTrue('scene.hydra.export_method = "HYDRA"' in code,
                        "le pilote demande encore l'export USD : Blender "
                        "démolit et reconstruit la scène entière à chaque "
                        "frame, et le procédural repart d'un état vide")
        self.assertFalse('export_method = "USD"' in code,
                         "l'export USD subsiste quelque part dans le pilote")

    def test_the_driver_names_the_manifest_for_the_plugin(self):
        # La fast path n'exporte rien, donc plus aucun hook ne peut y glisser
        # la prim Globe. Elle entre par le greffon de scene index, qui lit
        # cette variable — non posée, il rend la scène de l'hôte inchangée et
        # le globe est simplement absent, sans erreur nulle part.
        source = (pathlib.Path(__file__).with_name("render_usd.py")).read_text()
        code = "\n".join(l for l in source.splitlines()
                         if not l.lstrip().startswith("#"))
        self.assertTrue('os.environ["TUILE_MANIFEST"]' in code,
                        "le pilote ne nomme pas le manifeste : le rendu "
                        "sortira sans globe et rien ne le dira")

    def test_the_manifest_authors_an_attribute_too(self):
        # L'autre écrivain. Les deux doivent s'accorder, sinon USD refuse
        # l'export — ce qui vaut mieux que de s'accorder dans l'erreur.
        stage = (pathlib.Path(__file__).resolve().parents[2]
                 / "crates" / "tuile-usd" / "src" / "stage.rs")
        if not stage.is_file():
            self.skipTest("crates/tuile-usd/src/stage.rs absent")
        text = stage.read_text()
        authored = [l for l in text.splitlines()
                    if "tuile:cameras" in l and "writeln" not in l
                    and l.strip().startswith('"')]
        self.assertTrue(authored, "le manifeste ne nomme plus de caméra")
        for line in authored:
            self.assertNotIn("rel primvars:tuile:cameras", line, line)


class TheDriverCompilesOnce(unittest.TestCase):
    """338 secondes, puis 0,17 — et le premier chiffre ne doit se payer qu'une fois.

    Le PTX OptiX est déjà précompilé et livré dans l'image ; ce que le pilote
    fait ensuite, c'est le traduire en code machine pour la carte présente.
    Mesuré sur un L4 le 16 septembre 2026 : `lot 1/2: 322.49 s/frame`, puis
    `lot 2/2: 0.17 s/frame`. Sur trois tâches, c'est dix-sept minutes de
    compilation pour une minute de film.

    Ça ne peut pas être fait à la construction de l'image — le builder n'a pas
    de carte NVIDIA, et le résultat dépend de la carte et du pilote. OptiX sait
    en revanche le garder sur disque (`OPTIX_CACHE_PATH`), et ce disque-là ne
    survit pas à un conteneur. On le transporte donc.
    """

    JOB = pathlib.Path(__file__).with_name("render_job.sh")

    def test_the_cache_is_pulled_before_the_first_render(self):
        text = self.JOB.read_text()
        pull = text.index("JOB_OPTIX_CACHE_GET_URL")
        render = text.index("stdbuf -oL blender")
        self.assertLess(pull, render,
                        "le cache est tiré après le rendu : il ne sert à rien")
        self.assertIn('export OPTIX_CACHE_PATH=', text)

    def test_a_missing_cache_is_not_a_failure(self):
        # La première fois, il n'y a rien. Un rendu ne doit pas en mourir.
        text = self.JOB.read_text()
        block = text[text.index("OPTIX-CACHE-MISS") - 600:
                     text.index("OPTIX-CACHE-MISS") + 120]
        self.assertIn("OPTIX-CACHE-MISS", block)
        self.assertNotIn("exit 1", block)

    def test_only_one_task_writes_the_cache_back(self):
        text = self.JOB.read_text()
        put = text[text.index("JOB_OPTIX_CACHE_PUT_URL"):]
        put = put[:put.index("\nfi\n") + 4]
        self.assertIn('"$task_index" = "0"', put,
                      "toutes les tâches réécrivent le même cache : le "
                      "résultat dépendrait de l'ordre d'arrivée")

    def test_the_key_changes_with_the_image(self):
        source = (pathlib.Path(__file__).with_name("launch_job.py")).read_text()
        self.assertIn("cache/optix/", source)
        block = source[source.index("optix_key ="):source.index("optix_key =") + 200]
        self.assertIn("digest", block,
                      "la clef ne dépend pas de l'image : des noyaux changés "
                      "réutiliseraient un cache périmé")


class OneInvocationFromSceneToFilm(unittest.TestCase):
    """Des paramètres de scène au mp4, sans étape à taper.

        paramètres → bake (job A) → tuilepack sur R2
                   → render (job B) → segments → montage → mp4 sur R2

    Le montage a été une commande manuelle pendant quelques heures, et c'était
    une faute : aucune tâche ne voit tous les segments — voilà pourquoi il est
    hors du job — mais « hors du job » ne veut pas dire « à la main ».
    """

    def _source(self):
        return (pathlib.Path(__file__).with_name("launch_job.py")).read_text()

    def test_a_multi_task_render_assembles_itself(self):
        src = self._source()
        main = src[src.index("def main():"):]
        watch = main.index("ok, bad = gcp_watch(name, token=token)")
        after = main[watch:watch + 900]
        # `assemble(run` plutôt que `assemble(run)` : le montage reçoit
        # désormais la cadence, qui ne peut plus valoir 24 en dur.
        self.assertIn("assemble(run", after,
                      "le montage n'est pas enchaîné au rendu")
        self.assertIn("fps=", after,
                      "le montage monterait à 24 i/s quelle que soit la "
                      "cadence de la trajectoire")
        self.assertIn("bad == 0", after,
                      "on monterait des segments d'un rendu qui a échoué")

    def test_a_missing_pack_is_baked_first(self):
        src = self._source()
        main = src[src.index("def main():"):]
        self.assertIn("pack absent, cuisson d'abord", main)
        self.assertIn("args.pack = key", main,
                      "le rendu ne reprend pas le pack qui vient d'être cuit")

    def test_an_existing_pack_is_reused(self):
        # Recuire 1440 frames pour rendre deux fois le même plan, c'est treize
        # minutes et quelques dollars jetés.
        src = self._source()
        self.assertIn("def pack_exists(", src)
        main = src[src.index("def main():"):]
        self.assertIn("pack déjà cuit", main)

    def test_the_finished_film_is_opened(self):
        # CLAUDE.md : un rendu est une image, et chaque instrument qui remplace
        # le fait de la regarder a déjà menti ici.
        src = self._source()
        asm = src[src.index("def assemble("):]
        asm = asm[:asm.index("\ndef ")]
        self.assertIn('"open"', asm)


class ASuccessfulTaskExitsZero(unittest.TestCase):
    """Un `kill` qui rate ne doit pas transformer une réussite en échec.

    Le piège de sortie faisait `kill …; archive_everything`, et
    `archive_everything` lisait le code dans `$?` — c'est-à-dire celui du
    `kill`, pas celui du script. Dès que l'échantillonneur GPU s'était arrêté
    seul, `kill` échouait et la tâche se déclarait en erreur.

    Mesuré le 16 septembre 2026 sur une minute de film : `TASK-DONE 2/3
    (1 segments, frames 961:1440)` immédiatement suivi de `Container called
    exit(1)`. Les trois tâches avaient rendu leurs 1440 frames et déposé leurs
    segments ; l'exécution s'est déclarée `2✗`, et le montage automatique —
    gardé derrière « aucune tâche en échec » — ne s'est jamais lancé.
    """

    def test_a_dead_sampler_does_not_fail_the_task(self):
        # La forme exacte du script : un piège qui tue des processus déjà
        # morts, puis archive.
        script = """
        archive_everything() {
            local status="${1:-$?}"
            trap - EXIT
            exit $status
        }
        sampler() { sleep 0.01; }
        sampler & gpu_watch=$!
        wait "$gpu_watch" 2>/dev/null || true
        trap 'rc=$?; kill "$gpu_watch" 2>/dev/null || true; archive_everything "$rc"' EXIT
        true
        """
        done = subprocess.run(["bash", "-c", script], capture_output=True,
                              text=True, timeout=15)
        self.assertEqual(done.returncode, 0,
                         "une tâche réussie sort en erreur parce que le kill "
                         "d'un processus déjà mort a écrasé le code de sortie")

    def test_a_real_failure_still_fails(self):
        # Et l'inverse : un vrai échec ne doit pas être blanchi.
        script = """
        archive_everything() {
            local status="${1:-$?}"
            trap - EXIT
            exit $status
        }
        sampler() { sleep 30; }
        sampler & gpu_watch=$!
        trap 'rc=$?; kill "$gpu_watch" 2>/dev/null || true; archive_everything "$rc"' EXIT
        exit 3
        """
        done = subprocess.run(["bash", "-c", script], capture_output=True,
                              text=True, timeout=15)
        self.assertEqual(done.returncode, 3)

    def test_the_job_captures_rc_before_the_kill(self):
        text = (pathlib.Path(__file__).with_name("render_job.sh")).read_text()
        for line in text.splitlines():
            if "trap" in line and "archive_everything" in line:
                self.assertIn("rc=$?", line,
                              f"le code de sortie n'est pas capturé avant le "
                              f"ménage: {line}")
                if "kill" in line:
                    self.assertLess(line.index("rc=$?"), line.index("kill"),
                                    "le kill précède la capture du code")


class TheWatchOutlivesItsToken(unittest.TestCase):
    """Un rendu peut durer plus d'une heure ; un jeton Google, non.

    Le jeton était pris une fois au lancement et réutilisé pour tout le suivi.
    Mesuré le 16 septembre 2026 : « 401: Request had invalid authentication
    credentials » au bout d'une heure, en plein rendu d'une minute de film. Le
    lanceur est mort, et avec lui le montage automatique qui devait suivre —
    pendant que les trois tâches travaillaient toujours.
    """

    def test_the_watch_re_mints_before_the_hour(self):
        src = (pathlib.Path(__file__).with_name("launch_job.py")).read_text()
        watch = src[src.index("def gcp_watch("):]
        watch = watch[:watch.index("\ndef ")]
        self.assertIn("gcp_token()", watch,
                      "le suivi ne renouvelle jamais son jeton")
        self.assertIn("45 * 60", watch,
                      "le renouvellement doit précéder l'expiration à 60 min")


class TheBakeIsToldHowMuchMemoryItMayUse(unittest.TestCase):
    """`--resident-gb` doit atteindre la cuisson, pas seulement le rendu.

    Il ne l'atteignait pas. Le drapeau n'était posé que dans les deux branches
    `--engine hydra`, c'est-à-dire sur le chemin du rendu ; une cuisson tournait
    donc au défaut du code — 4 GiB — dans un conteneur qui en a 32.

    Et la panne ne ressemblait pas à une panne de configuration. Mesuré le
    19 septembre 2026 : `resident_gib=3.76` collé au plafond, et
    `loads_started=134720` pour 234 tuiles sélectionnées — quarante-cinq
    chargements par tuile. Le cache évinçait ce dont la frame avait besoin, la
    traversée le redemandait, et la frame ne convergeait jamais. Ce qui
    remontait, c'était « frame did not converge within 120s », qui accuse la
    traversée et tait la seule ligne qui comptait.
    """

    def _args(self, **over):
        base = dict(frames="1:2880", trajectory="pyrenees:2:24:20000:0.20",
                    viewport="1920x1440", sse=3.0, imagery=3954, terrain=0,
                    imagery_boost=1, resident_gb=16)
        base.update(over)
        return types.SimpleNamespace(**base)

    def _env(self, **over):
        return launch_job.bake_env(self._args(**over), "jeton",
                                   "https://put/pack", "https://put/scene",
                                   "https://put/logs")

    def test_the_budget_travels(self):
        self.assertEqual(self._env()["TUILE_RESIDENT_BUDGET_GB"], "16")

    def test_the_flag_is_what_decides_it(self):
        # Sinon le test ci-dessus passerait sur une constante en dur, qui est
        # exactement l'erreur d'avant sous un autre nom.
        self.assertEqual(
            self._env(resident_gb=24)["TUILE_RESIDENT_BUDGET_GB"], "24")

    def test_the_budget_is_not_the_container_s(self):
        # Un budget égal à la mémoire du conteneur ne laisse rien au décodage,
        # au cache de fetch ni au pack — qui vivent tous à côté, et dont les
        # deux derniers sont en tmpfs, donc en RAM eux aussi.
        self.assertLess(int(self._env()["TUILE_RESIDENT_BUDGET_GB"]), 32)


class TheCadenceIsConfigurableAndNamed(unittest.TestCase):
    """La cadence se règle, et elle change le nom du pack.

    Elle ne se réglait pas : `render_job.sh` tenait `JOB_FPS` pour 24, en dur,
    et s'en servait deux fois — pour calculer les poses du manifeste, et pour
    le `-framerate` de ffmpeg sur chaque segment. Une trajectoire à 60 aurait
    donc produit des poses introuvables dans le pack (échec bruyant) et, si
    elle y avait survécu, un film de cinq minutes pour deux minutes de vol
    (échec muet).

    Elle vit dans la chaîne de trajectoire et nulle part ailleurs. Un drapeau
    `--fps` à côté serait un second endroit pour le même nombre, donc un
    second endroit pour qu'ils divergent.
    """

    def _args(self, trajectory, frames="1:7200"):
        return types.SimpleNamespace(
            frames=frames, trajectory=trajectory, viewport="1920x1440",
            sse=3.0, imagery=3954, terrain=0, imagery_boost=1, resident_gb=16)

    def test_the_cadence_comes_from_the_trajectory(self):
        self.assertEqual(launch_job.fps_of("pyrenees:2:60:10000:0.10"), 60)
        self.assertEqual(launch_job.fps_of("pyrenees:2:24:20000:0.20"), 24)

    def test_a_whole_cadence_stays_whole(self):
        # `render_usd.py --fps` est `type=int` : "60.0" y lève. Mesuré le
        # 19 septembre 2026 — le pod a reçu `JOB_FPS=60.0`, seul le générateur
        # de manifeste l'a lu (il accepte les flottants), et Blender a gardé sa
        # cadence de scène par défaut. Les 7200 poses étaient justes, le
        # conteneur était estampillé 24, et le film durait cinq minutes.
        self.assertIsInstance(launch_job.fps_of("pyrenees:2:60:10000:0.10"), int)
        self.assertEqual(str(launch_job.fps_of("pyrenees:2:60:10000:0.10")), "60")

    def test_the_renderer_is_given_the_cadence_too(self):
        # Le manifeste ET le rendu. `JOB_FPS` n'atteignait que le premier :
        # les poses étaient à 60, l'encodage à 24.
        script = (pathlib.Path(__file__).with_name("render_job.sh")).read_text()
        # L'invocation, pas la mention en commentaire plus haut.
        blender = script[script.index("-P /opt/render/render_usd.py"):]
        self.assertIn('--fps "$JOB_FPS"', blender[:600],
                      "Blender garderait scene.render.fps à 24")

    def test_a_trajectory_that_states_none_keeps_the_default(self):
        # `orbit` compte des frames, pas des minutes ; `zoom` non plus.
        self.assertEqual(launch_job.fps_of("orbit:1440:2.17:42.52:8000:5000"), 24)
        self.assertEqual(launch_job.fps_of("zoom:64"), 24)
        self.assertEqual(launch_job.fps_of(""), 24)
        self.assertEqual(launch_job.fps_of("pyrenees:2"), 24)
        self.assertEqual(launch_job.fps_of("pyrenees:2:pas-un-nombre"), 24)

    def test_the_key_changes_with_the_cadence(self):
        # Sans ça, deux films de cadences différentes partageraient un pack et
        # le second lirait les poses du premier.
        slow = launch_job.pack_key(self._args("pyrenees:2:24:10000:0.10"))
        fast = launch_job.pack_key(self._args("pyrenees:2:60:10000:0.10"))
        self.assertNotEqual(slow, fast)

    def test_the_render_is_told_the_cadence(self):
        src = (pathlib.Path(__file__).with_name("launch_job.py")).read_text()
        self.assertIn('env["JOB_FPS"] = str(fps_of(args.trajectory))', src,
                      "le job de rendu retomberait sur son défaut de 24")


class TheFrameRangeMatchesTheTrajectory(unittest.TestCase):
    """`--frames` et la trajectoire décrivent le même film, ou on le dit.

    Les deux sont des arguments séparés et rien ne les obligeait à s'accorder.
    Dans le sens long c'est une erreur — la bande n'a pas ces poses. Dans le
    sens court c'est légitime (on cuit une tranche exprès) et c'est là qu'est
    le danger : 7200 frames cuites `1:2880`, ce sont quarante-huit secondes
    livrées pour deux minutes demandées, sans qu'aucun compteur ne s'en plaigne.
    """

    def test_a_cadence_change_changes_the_count(self):
        self.assertEqual(launch_job.frames_of("pyrenees:2:24:10000:0.10"), 2880)
        self.assertEqual(launch_job.frames_of("pyrenees:2:60:10000:0.10"), 7200)

    def test_orbit_states_its_own_count(self):
        self.assertEqual(
            launch_job.frames_of("orbit:1440:2.17:42.52:8000:5000"), 1440)

    def test_a_trajectory_that_states_nothing_is_not_guessed(self):
        self.assertIsNone(launch_job.frames_of("zoom:64"))

    def test_asking_past_the_end_of_the_tape_is_refused(self):
        with self.assertRaises(SystemExit):
            launch_job.check_frames("pyrenees:2:24:10000:0.10", "1:7200")

    def test_a_partial_bake_is_allowed_and_said(self):
        said = []
        launch_job.check_frames("pyrenees:2:60:10000:0.10", "1:2880",
                                say=said.append)
        self.assertTrue(said, "une cuisson amputée passerait en silence")
        self.assertIn("2880", said[0])
        self.assertIn("7200", said[0])

    def test_the_whole_film_says_nothing(self):
        said = []
        launch_job.check_frames("pyrenees:2:60:10000:0.10", "1:7200",
                                say=said.append)
        self.assertEqual(said, [])

