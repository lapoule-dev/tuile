#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The farm job scripts' contract: `render_job.sh` and `bake_job.sh`.

    python3 -m unittest discover -s integrations/blender -p 'test_*.py'

The launchers are Rust now (`tuile-farm`: `launch-job`, with its own tests);
what stays here is what only the scripts decide — how a task cuts its slice,
what it archives and when, how it moves its bytes through `tuile-farm`, and
that a dying render gives the machine back. Several of these run the scripts'
functions for real rather than reading them.
"""

import importlib.util
import io
import pathlib
import re
import subprocess
import sys
import types
import urllib.error
import unittest
import unittest.mock

# Jamais de bytecode pour ce que ce fichier teste.
#
# `importlib` réutilise un `.pyc` quand la source a la même taille ET la même
# seconde de mtime. Mesuré ici : remplacer `GPU-HOST-BROKEN` par
# `GPU-HOST-BORKED` — six lettres contre six lettres, dans la même seconde —
# laissait le test lire l'ancien bytecode et affirmer le contraire de ce que
# le fichier disait. Un test qui lit une version périmée de ce qu'il teste est
# pire que pas de test : il rassure.
sys.dont_write_bytecode = True





_FARM = None


def farm_binary():
    """The real `tuile-farm`, built once for the whole run.

    The job scripts call it for every byte they move, so a test of their I/O
    runs it rather than a stand-in: a fake `curl` once said "uploaded" for
    eleven runs that deposited nothing."""
    global _FARM
    if _FARM is None:
        root = pathlib.Path(__file__).resolve().parents[2]
        subprocess.run(["cargo", "build", "-q", "-p", "tuile-farm"], cwd=root, check=True)
        _FARM = root / "target" / "debug" / "tuile-farm"
    return _FARM



class Archive(unittest.TestCase):
    """Ce qu'un run dépose, et pourquoi rien n'est optionnel."""


    def test_the_job_archives_on_every_exit_path(self):
        """Le run dont on a le plus besoin des journaux est celui qui a
        échoué — donc les journaux ne peuvent pas dépendre de la réussite,
        ni d'un drapeau. Le job les nomme lui-même, sous son préfixe."""
        script = pathlib.Path(__file__).with_name("render_job.sh").read_text()
        final = script[script.index("archive_everything() {"):]
        final = final[:final.index("\n}\n")]
        for name in ("logs.tar.gz", "trace.tar.gz", "profile.tar.gz"):
            self.assertIn(f"ship {name}", final, f"{name} ne part pas à la sortie")
        self.assertIn("trap 'rc=$?; archive_everything", script)










class NoPythonInTheImage(unittest.TestCase):
    """Le job ne doit pas appeler python3 : l'image n'en a pas.

    Blender embarque le sien et rien d'autre n'est installé. Deux contrôles
    successifs ont appelé `python3`, n'ont rien produit, et ont quand même
    fait échouer le pod — pour la bonne conclusion et la mauvaise raison.
    C'est le genre de chance qui cesse exactement quand elle compte."""

    def test_the_job_never_calls_a_python_that_is_not_there(self):
        script = (pathlib.Path(__file__).parent
                  / "render_job.sh").read_text()
        offending = [l.strip() for l in script.splitlines()
                     if "python3" in l and not l.strip().startswith("#")]
        self.assertEqual(offending, [],
                         "l'image n'a pas de python3 ; utilise `blender "
                         "--python-expr`, ou pas d'interpréteur du tout")




class DepositAsYouGo(unittest.TestCase):
    """Un pod repris en cours de route doit avoir déjà déposé ce qu'il a fait."""

    def test_one_url_per_segment_so_nothing_waits_for_the_concat(self):
        """Attendre le montage, c'est ce qui a fait perdre 1440 frames déjà
        rendues : le pod a été repris et le téléversement n'avait pas eu lieu."""
        script = (pathlib.Path(__file__).parent
                  / "render_job.sh").read_text()
        # Le chemin de RÉUSSITE, pas seulement le mot : `SEG-UP-FAILED`
        # contient « SEG-UP », et un test qui s'en contente passe même si le
        # téléversement a été retiré. Et DANS la boucle des segments, pas
        # après le montage.
        loop = script[script.index("for i in $(seq 0 $((jobs - 1))); do\n    if [ ! -s"):]
        loop = loop[:loop.index("\ndone\n")]
        self.assertIn('"$FARM" put "$outdir/seg$i.mp4" "$(run_key "seg$g.mp4")"', loop)

    def test_the_logs_go_up_while_it_runs_not_only_at_the_end(self):
        """Le `trap` couvre toutes les façons dont ce script décide de
        s'arrêter. Il ne couvre pas celle dont un pod meurt vraiment :
        SIGKILL, pas de trap, rien d'écrit."""
        script = (pathlib.Path(__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("flush_logs", script)
        self.assertIn("JOB_ARCHIVE_EVERY", script)


class PackedRender(unittest.TestCase):
    """Un rendu nourri par un pack ne doit porter aucun jeton."""


    def test_the_job_script_drops_the_token_once_a_pack_is_in_hand(self):
        """Un jeton posé à côté d'un pack est un jeton qu'on peut encore
        atteindre. Le retirer est ce qui transforme une intention en fait."""
        script = (pathlib.Path(__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("unset TUILE_ION_TOKEN", script)


class FourGpus(unittest.TestCase):
    """Storm ne peut pas utiliser quatre GPU, et le job doit le savoir."""

    def test_the_job_probes_optix_devices_for_the_cycles_path(self):
        """Compter les cartes que le pilote voit n'est pas la même question
        que « sur combien le moteur sait poser du travail ». Répondre à la
        question facile est ce qui a laissé seize processus se partager un
        seul GPU pendant que la sonde en annonçait quatre."""
        script = (pathlib.Path(__file__).parent
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
        script = (pathlib.Path(__file__).parent
                  / "render_job.sh").read_text()
        self.assertTrue('CYCLES_DEVICE="$CYCLES_BACKEND"' in script,
                        "le délégué n'est pas informé de son device")

    def test_the_job_reports_whether_the_gpus_actually_worked(self):
        """Un run qui n'a utilisé qu'un GPU sur quatre n'est pas un run lent,
        c'est un run cassé — et ça ne doit pas demander qu'un humain regarde
        une capture d'écran."""
        script = (pathlib.Path(__file__).parent
                  / "render_job.sh").read_text()
        self.assertIn("GPU-UNDERUSED", script)




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
        self.assertIn('run_key "seg$g.mp4"', text)
        # Et le reçu nomme exactement ces numéros-là.
        self.assertIn('segs=$(seq -s, "$seg_base" $((seg_base + jobs - 1)))', text)

    def test_each_task_ships_its_own_logs(self):
        # Une seule clef de journaux pour N tâches, et le survivant est celui
        # qui a fini en dernier — jamais celui qui a échoué.
        text = self.JOB.read_text()
        self.assertIn('[ "$task_count" -gt 1 ] && task_tag="-t$task_index"', text)
        ship = text[text.index("ship() {"):]
        self.assertIn('${name%.tar.gz}${task_tag}.tar.gz', ship[:ship.index("\n}\n")])

    def test_the_film_is_assembled_from_receipts_not_by_every_task(self):
        # Chaque tâche dépose son reçu PUIS demande le film ; seule celle qui
        # trouve tous les reçus monte, les autres disent TASK-DONE.
        text = self.JOB.read_text()
        receipt = text.index('"$FARM" receipt "$JOB_RUN_PREFIX"')
        assemble = text.index('"$FARM" assemble "$JOB_RUN_PREFIX" "$task_count"')
        self.assertLess(receipt, assemble, "le reçu doit précéder la demande de film")
        verdict = text[assemble:text.index("esac", assemble)]
        self.assertIn("FILM-UP*) echo RENDER-DONE", verdict)
        self.assertIn("TASK-DONE", verdict)
        # Et plus aucun ffmpeg : le montage est en Rust.
        self.assertNotIn("ffmpeg", "\n".join(
            l for l in text.splitlines() if not l.lstrip().startswith("#")))




class OneArchivePerTask(unittest.TestCase):
    """N tâches, N jeux de journaux.

    Une seule clé partagée et le survivant est celui qui a fini en dernier —
    jamais celui qui a échoué, qui est pourtant le seul qu'on voulait lire.
    """

    JOB = pathlib.Path(__file__).with_name("render_job.sh")

    def _ship_keys(self, task_index, task_count):
        """Les clefs sous lesquelles `ship` dépose, pour une tâche donnée."""
        text = self.JOB.read_text()
        ident = text[text.index('task_tag=""'):text.index('FARM="${TUILE_FARM')]
        body = text[text.index("ship() {"):]
        body = body[:body.index("\n}\n") + 3]
        import tempfile as _tempfile
        with _tempfile.TemporaryDirectory() as tmp:
            (pathlib.Path(tmp) / "job.log").write_text("x\n")
            script = (f'outdir="{tmp}"\ntask_index={task_index}\ntask_count={task_count}\n'
                      + ident
                      + f'FARM="{farm_binary()}"\nexport TUILE_STORE_DIR="{tmp}/store"\n'
                      'JOB_RUN_PREFIX=renders/r1\n'
                      "run_key() { printf '%s/%s' \"$JOB_RUN_PREFIX\" \"$1\"; }\n"
                      + body
                      + "ship logs.tar.gz 'job.log'\n")
            subprocess.run(["bash", "-c", script], capture_output=True, text=True)
            store = pathlib.Path(tmp) / "store" / "renders" / "r1"
            return sorted(p.name for p in store.iterdir()) if store.exists() else []

    def test_a_lone_task_needs_no_suffixed_archive(self):
        self.assertEqual(self._ship_keys(0, 1), ["logs.tar.gz"])

    def test_every_task_gets_its_own_logs(self):
        self.assertEqual(self._ship_keys(2, 3), ["logs-t2.tar.gz"])

    def test_segments_are_numbered_across_every_task(self):
        # Trois tâches de quatre processus font douze segments, numérotés 0..11
        # d'un bout à l'autre — et non 0..3 trois fois, qui se recouvriraient.
        text = self.JOB.read_text()
        self.assertIn("seg_base=$((task_index * jobs))", text)
        self.assertIn("g=$((seg_base + i))", text)




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
            # The real uploader, on a directory store: what arrives is what a
            # bucket would have received.
            script = (
                f'outdir="{tmp}"\n'
                f'FARM="{farm_binary()}"\n'
                f'export TUILE_STORE_DIR="{tmp}/store"\n'
                'JOB_RUN_PREFIX=renders/r1\n'
                'task_tag=""\n'
                "run_key() { printf '%s/%s' \"$JOB_RUN_PREFIX\" \"$1\"; }\n"
                + body
                + grower
                + "ship logs.tar.gz 'log-s*.txt' 'job.log'\n"
                "wait\n")
            done = subprocess.run(["bash", "-c", script],
                                  capture_output=True, text=True)
            landed = out / "store" / "renders" / "r1" / "logs.tar.gz"
            return done.stdout, landed.exists() and landed.stat().st_size > 0

    def test_a_file_growing_under_tar_still_ships(self):
        stdout, made = self._ship(grow=True)
        self.assertTrue(made, "aucune archive arrivée dans le store")
        self.assertIn("ARCHIVE-UP renders/r1/logs.tar.gz", stdout,
                      f"rien n'a été téléversé — sortie: {stdout!r}")
        self.assertNotIn("ARCHIVE-TAR-FAILED", stdout)

    def test_a_quiet_directory_ships_too(self):
        stdout, made = self._ship(grow=False)
        self.assertTrue(made)
        self.assertIn("ARCHIVE-UP renders/r1/logs.tar.gz", stdout)

    def test_a_failure_is_no_longer_swallowed_by_dev_null(self):
        # Le flush envoyait tout dans /dev/null : la panne était muette.
        text = self.JOB.read_text()
        flush = text[text.index("flush_logs() {"):]
        flush = flush[:flush.index("\n}\n")]
        self.assertNotIn("> /dev/null 2>&1", flush)
        self.assertIn("*FAILED*", flush)


class EveryCrashReportComesBack(unittest.TestCase):
    """Chaque processus Blender a son rapport de plantage, et il remonte.

    Blender écrit sa pile dans `<tmp>/blender.crash.txt` — un seul chemin pour
    tous les processus de la machine — et rien ne l'envoyait. Mesuré le 23
    septembre : deux processus sur quatre ont planté sur leur deuxième frame,
    et il n'est revenu que la ligne « Writing: /tmp/blender.crash.txt ».

    Ce test exécute le vrai `ship` avec les motifs exacts de l'archivage de
    sortie, et ouvre l'archive arrivée dans le store.
    """

    JOB = pathlib.Path(__file__).with_name("render_job.sh")

    def test_each_process_writes_its_crash_report_under_outdir(self):
        text = self.JOB.read_text()
        self.assertIn('TMPDIR="$outdir/tmp-s$i"', text)
        self.assertIn('mkdir -p "$outdir/tmp-s$i"', text)

    def test_the_crash_reports_travel_with_the_logs(self):
        import tarfile
        import tempfile as _tempfile
        text = self.JOB.read_text()
        start = text.index("ship() {")
        body = text[start:text.index("\n}\n", start) + len("\n}\n")]
        exit_body = text[text.index("archive_everything() {"):]
        call = next(line.strip() for line in exit_body.splitlines()
                    if line.strip().startswith("ship logs.tar.gz"))
        with _tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            (out / "log-s0.txt").write_text("frame 3991: début\n")
            (out / "job.log").write_text("job\n")
            (out / "tmp-s1").mkdir()
            (out / "tmp-s1" / "blender.crash.txt").write_text("# backtrace\n")
            script = (
                f'outdir="{tmp}"\n'
                f'FARM="{farm_binary()}"\n'
                f'export TUILE_STORE_DIR="{tmp}/store"\n'
                'JOB_RUN_PREFIX=renders/r1\n'
                'task_tag=""\n'
                "run_key() { printf '%s/%s' \"$JOB_RUN_PREFIX\" \"$1\"; }\n"
                + body + call + "\n")
            done = subprocess.run(["bash", "-c", script],
                                  capture_output=True, text=True)
            landed = out / "store" / "renders" / "r1" / "logs.tar.gz"
            self.assertTrue(landed.exists(), done.stdout + done.stderr)
            with tarfile.open(landed) as tar:
                names = tar.getnames()
            self.assertIn("tmp-s1/blender.crash.txt", names)
            self.assertIn("log-s0.txt", names)


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
            # The real uploader, on a directory store: what arrives is what a
            # bucket would have received.
            script = (
                f'outdir="{tmp}"\n'
                f'FARM="{farm_binary()}"\n'
                f'export TUILE_STORE_DIR="{tmp}/store"\n'
                'JOB_RUN_PREFIX=renders/r1\n'
                'task_tag=""\n'
                "run_key() { printf '%s/%s' \"$JOB_RUN_PREFIX\" \"$1\"; }\n"
                + body
                + grower
                + "ship logs.tar.gz 'log-s*.txt' 'job.log'\n"
                "wait\n")
            done = subprocess.run(["bash", "-c", script],
                                  capture_output=True, text=True)
            landed = out / "store" / "renders" / "r1" / "logs.tar.gz"
            return done.stdout, landed.exists() and landed.stat().st_size > 0

    def test_a_file_growing_under_tar_still_ships(self):
        stdout, made = self._ship(grow=True)
        self.assertTrue(made, "aucune archive arrivée dans le store")
        self.assertIn("ARCHIVE-UP renders/r1/logs.tar.gz", stdout,
                      f"rien n'a été téléversé — sortie: {stdout!r}")
        self.assertNotIn("ARCHIVE-TAR-FAILED", stdout)

    def test_a_quiet_directory_ships_too(self):
        stdout, made = self._ship(grow=False)
        self.assertTrue(made)
        self.assertIn("ARCHIVE-UP renders/r1/logs.tar.gz", stdout)

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


class ThePackCarriesItsOwnFlight(unittest.TestCase):
    """Le rendu doit rejouer la bande qui a cuit le pack, pas la refabriquer.

    Un pack répond à une caméra par la pose, à un mètre et un milliradian. Le
    20 septembre 2026, un rendu lancé avec la même chaîne `pyrenees:...` que la
    cuisson est mort en neuf secondes sur les trois tâches : `pyrenees-tape`
    penche désormais de 20° sur la verticale, la cuisson avait reçu une bande
    nadir archivée, et l'écart lu était 0,349066 rad — vingt degrés, à six
    millimètres près sur la position."""



    def test_the_render_script_prefers_the_tape_over_the_trajectory(self):
        script = (pathlib.Path(__file__).parent / "render_job.sh").read_text()
        tape = script.index('"$FARM" get "$JOB_TAPE_KEY" /tmp/traj.mcap')
        generated = script.index("/opt/tuile/bin/pyrenees-tape /tmp/traj.mcap")
        self.assertLess(tape, generated,
                        "la bande fournie doit l'emporter sur la générée")
        # Et une seule étape en sort, quelle que soit la source.
        self.assertEqual(script.count("/opt/tuile/bin/tape-to-stage"), 1)

    def test_both_scripts_take_a_supplied_tape(self):
        here = pathlib.Path(__file__).parent
        for name in ("bake_job.sh", "render_job.sh"):
            with self.subTest(script=name):
                self.assertIn("JOB_TAPE_KEY", (here / name).read_text(),
                              f"{name} ignore une bande fournie — le pack et "
                              "l'image ne décriraient pas le même tournage")


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
        pull = text.index('"$FARM" get "$JOB_OPTIX_CACHE_KEY"')
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
        put = text[text.index('"$FARM" put "$outdir/optix-cache-out.tar.gz"') - 300:]
        put = put[:put.index("\nfi\n") + 4]
        self.assertIn('"$task_index" = "0"', put,
                      "toutes les tâches réécrivent le même cache : le "
                      "résultat dépendrait de l'ordre d'arrivée")





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



    def test_the_renderer_is_given_the_cadence_too(self):
        # Le manifeste ET le rendu. `JOB_FPS` n'atteignait que le premier :
        # les poses étaient à 60, l'encodage à 24.
        script = (pathlib.Path(__file__).with_name("render_job.sh")).read_text()
        # L'invocation, pas la mention en commentaire plus haut.
        blender = script[script.index("-P /opt/render/render_usd.py"):]
        self.assertIn('--fps "$JOB_FPS"', blender[:600],
                      "Blender garderait scene.render.fps à 24")









class TheTwoJobsAgreeOnTheCadence(unittest.TestCase):
    """Cuisson et rendu doivent échantillonner la même bande.

    Sinon le pack décrit un tournage et l'image en montre un autre, et les deux
    jobs annoncent une réussite. Le rendu avait DEUX sources — `$JOB_FPS` pour
    la scène et l'encodage, `${p2:-24}` pour la bande — et la cuisson n'avait
    pas `JOB_FPS` du tout.
    """

    def _scripts(self):
        here = pathlib.Path(__file__).parent
        return ((here / "bake_job.sh").read_text(),
                (here / "render_job.sh").read_text())

    def test_both_derive_the_cadence_the_same_way(self):
        bake, render = self._scripts()
        block = 'pyrenees) JOB_FPS="${_p2:-24}" ;;'
        self.assertIn(block, bake)
        self.assertIn(block, render)

    def test_neither_reads_the_cadence_from_a_second_place(self):
        # `${p2:-24}` sur l'appel au générateur de bande était la seconde
        # source : elle ne s'accordait avec `$JOB_FPS` que par coïncidence.
        for name, script in zip(("bake_job.sh", "render_job.sh"), self._scripts()):
            self.assertNotIn('"${p2:-24}"', script, name)

    def test_the_tape_is_built_at_that_cadence(self):
        for name, script in zip(("bake_job.sh", "render_job.sh"), self._scripts()):
            # L'invocation, pas les commentaires qui la nomment.
            line = [l for l in script.splitlines()
                    if "/opt/tuile/bin/pyrenees-tape" in l]
            self.assertTrue(line, name)
            nxt = script.splitlines()[script.splitlines().index(line[0]) + 1]
            self.assertIn('"$JOB_FPS"', nxt, f"{name}: {nxt}")



class ASuppliedTapeWinsOverAGeneratedOne(unittest.TestCase):
    """Recuire un pack sur son propre tracé.

    Les générateurs évoluent : `pyrenees-tape` sortait une polyligne quand les
    premiers films ont été tournés et sort une spline aujourd'hui. La même
    chaîne d'arguments ne décrit donc plus le même vol, et un pack recuit
    depuis elle ne se compare pas à celui d'avant. Or le suspect EST le pack —
    la sélection y est figée — donc la seule question qui se pose demande de
    survoler exactement la même mer.
    """

    def _args(self, **over):
        base = dict(frames="1:2880", trajectory="pyrenees:2:24:50000:0.40",
                    viewport="3840x2880", sse=3.0, imagery=0, terrain=0,
                    imagery_boost=1, resident_gb=16, tape=None)
        base.update(over)
        return types.SimpleNamespace(**base)




    def test_the_job_prefers_it_over_the_trajectory(self):
        script = (pathlib.Path(__file__).with_name("bake_job.sh")).read_text()
        head = script[script.index('if [ -n "${JOB_TAPE_KEY:-}"'):]
        # La bande fournie doit être testée AVANT la génération, sinon elle ne
        # l'emporte sur rien.
        self.assertLess(head.index('"$FARM" get'), head.index("pyrenees-tape"))
        self.assertIn("TAPE-DOWNLOAD-FAILED", head,
                      "un téléchargement raté cuirait une bande vide")



class TheTapeIsKeptBesideThePack(unittest.TestCase):
    """Bande, pack, film : les trois doivent se retrouver ensemble.

    La bande n'était conservée nulle part. Le job la fabriquait dans le
    conteneur, cuisait avec, et la jetait — pack, digest et film archivés, le
    tracé non, alors qu'il est la seule chose dont les trois découlent.

    Mesuré le 19 septembre 2026 : recuire le premier film sur son propre tracé
    a demandé de redescendre un gigaoctet de pack et d'en extraire les caméras
    frame par frame. Ça a marché parce qu'un pack enregistre la caméra de
    chaque frame ; ça n'aurait pas dû être nécessaire, et ça ne marcherait pas
    pour un tracé dont aucun pack n'a survécu.
    """

    def _args(self, **over):
        base = dict(frames="1:2880", trajectory="pyrenees:2:24:50000:0.40",
                    viewport="3840x2880", sse=3.0, imagery=0, terrain=0,
                    imagery_boost=1, resident_gb=16, tape=None)
        base.update(over)
        return types.SimpleNamespace(**base)



    def test_the_job_deposits_it(self):
        script = (pathlib.Path(__file__).with_name("bake_job.sh")).read_text()
        self.assertIn('"$FARM" put "$tape" "$JOB_PACK_KEY.mcap"', script)
        # Après la cuisson : déposer une bande alors que le pack a échoué
        # laisserait un tracé sans rien à quoi le rattacher.
        self.assertGreater(script.index('"$FARM" put "$tape"'),
                           script.index('"$FARM" put "$pack" "$JOB_PACK_KEY"'))

    def test_a_supplied_tape_is_deposited_too(self):
        # Sinon le trou se rouvre au coup d'après : un pack recuit n'aurait pas
        # son tracé, et le suivant devrait à nouveau l'extraire à la main.
        script = (pathlib.Path(__file__).with_name("bake_job.sh")).read_text()
        deposit = script[script.index('"$FARM" put "$tape"') - 60:]
        self.assertNotIn("JOB_TAPE_KEY", deposit.split("fi")[0],
                         "le dépôt ne doit pas dépendre de l'origine de la bande")




