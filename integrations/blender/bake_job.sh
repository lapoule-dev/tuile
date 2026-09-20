#!/bin/bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Job A: bakes a camera path into ONE pack and deposits it on R2.
#
# The other half of the pair. This job is the only one that holds a Cesium ion
# token, the only one that talks to the network at length, and the only one
# that needs no GPU at all. `render_job.sh` then reads what this leaves — no
# token, no fetches, no traversal — which is the whole reason the pipeline was
# cut in two.
#
#   JOB_TRAJECTORY     orbit:frames:lon:lat:radius_m:alt_m, or zoom:frames
#   JOB_FRAMES         "1:1440" (inclusive)
#   JOB_VIEWPORT       "1280x960"   (the selection depends on it: a pack is
#                      the bake of the viewport it was made for)
#   JOB_SSE            screen-space error target (default 3)
#   JOB_PACK_PUT_URL   presigned PUT: where the finished pack goes
#   JOB_TAPE_PUT_URL   presigned PUT: the tape this pack was baked from, beside
#                      it. Without it a pack cannot be re-cooked on its own
#                      path — see below.
#   JOB_SCENE_PUT_URL  presigned PUT: the scene digest, as one line, beside the
#                      pack. It cannot be derived from the parameters — it also
#                      covers the RESOLVED traversal settings, which only this
#                      job knows — and a render needs it to check that the pack
#                      it was handed is a bake of the globe it means to draw.
#   JOB_LOGS_PUT_URL   presigned PUT: logs.tar.gz, on every exit path and
#                      every JOB_ARCHIVE_EVERY seconds while it runs
#   TUILE_ION_TOKEN    required, never printed
#   TUILE_*            every traversal knob, honoured by the same code the
#                      render honours — a pack is the bake of its settings
#
# Contract: a pack that is not deposited is a bake that did not happen. The
# exit code says so, and BAKE-KEY on stdout names the object a renderer should
# be given.
set -uo pipefail

JOB_FRAMES="${JOB_FRAMES:?JOB_FRAMES requis (ex: 1:1440)}"
JOB_TRAJECTORY="${JOB_TRAJECTORY:?JOB_TRAJECTORY requis (ex: orbit:1440:2.17:42.52:8000:5000)}"
JOB_VIEWPORT="${JOB_VIEWPORT:-1280x960}"
JOB_SSE="${JOB_SSE:-3}"
: "${TUILE_ION_TOKEN:?TUILE_ION_TOKEN requis — une cuisson ne cuit pas sans}"

outdir="${JOB_OUTDIR:-/out}"
mkdir -p "$outdir"
JOB_LOG="$outdir/job.log"
exec > >(tee -a "$JOB_LOG") 2>&1

# Cloud Run met /tmp en MÉMOIRE, et la mémoire est la ressource que cette
# cuisson dépense. Le déversoir du writer et le pack fini y vivent tous deux,
# donc ils comptent deux fois dans la limite du conteneur — d'où un plancher
# mémoire dimensionné pour deux fois le pack attendu, et non pour le pic du
# processus.
pack="$outdir/scene.tuilepack"
tape="$outdir/traj.mcap"

# Le même expéditeur que le job de rendu, et les mêmes raisons.
#
# Les motifs arrivent NON développés et c'est le shell qui doit les résoudre,
# dans $outdir : `ls "$@"` ne fait pas de glob, et le conteneur n'a pas de
# WORKDIR. Onze runs de rendu n'ont déposé aucune archive avant que ce soit
# compris ; ce job naît avec la version qui marche.
ship() {
    local name="$1" url="$2"; shift 2
    [ -n "$url" ] || return 0
    local files pat f
    files=$(cd "$outdir" 2>/dev/null && for pat in "$@"; do
                for f in $pat; do [ -e "$f" ] && printf '%s\n' "$f"; done
            done)
    [ -n "$files" ] || return 0
    local status=0
    tar czf "$outdir/$name" -C "$outdir" $files 2> "$outdir/.tar.err" || status=$?
    # `tar` rend 1 quand un fichier a changé pendant la lecture — job.log est
    # écrit par `tee` pendant qu'on l'archive. L'archive reste complète.
    if [ "$status" -ge 2 ] || [ ! -s "$outdir/$name" ]; then
        echo "ARCHIVE-TAR-FAILED $name (tar=$status)"
        return 1
    fi
    if curl -fsS -T "$outdir/$name" "$url" > /dev/null; then
        echo "ARCHIVE-UP $name ($(du -h "$outdir/$name" | cut -f1))"
    else
        echo "ARCHIVE-UP-FAILED $name"
        return 1
    fi
}

flush_logs() {
    [ -n "${JOB_LOGS_PUT_URL:-}" ] || return 0
    while sleep "${JOB_ARCHIVE_EVERY:-300}"; do
        out=$(ship logs.tar.gz "$JOB_LOGS_PUT_URL" 'job.log' 2>&1)
        case "$out" in *FAILED*) echo "$out" ;; esac
    done
}

archive_everything() {
    local status=$?
    trap - EXIT
    ship logs.tar.gz "${JOB_LOGS_PUT_URL:-}" 'job.log' 'profile'
    exit $status
}
trap archive_everything EXIT
flush_logs &
log_flusher=$!

echo "bake: frames $JOB_FRAMES, viewport $JOB_VIEWPORT, sse $JOB_SSE"

#   orbit:frames:lon:lat:radius_m:alt_m     (défauts après le genre)
#   pyrenees:minutes:fps:alt_m:offset_deg   (boucle nadir autour du massif)
#   zoom:frames_each_way
#
# Les mêmes genres, avec les mêmes défauts, que dans `render_job.sh` : la
# cuisson et le rendu doivent produire EXACTEMENT la même trajectoire, sinon le
# pack décrit un tournage et l'image en montre un autre — et les deux jobs
# annoncent une réussite.
# La cadence, une seule valeur pour tout le job.
#
# `JOB_FPS` fait autorité. À défaut elle se lit dans la trajectoire, qui la
# porte en deuxième position — et pour le seul genre `pyrenees` : `orbit` met
# une longitude à cette place, et la lire comme une cadence donnerait un film
# à deux images par seconde sans que rien ne s'en plaigne.
#
# Le lanceur pose la même valeur, calculée par `fps_of`. Deux endroits pour un
# même nombre, c'est deux endroits pour qu'ils divergent : un test tient les
# deux dérivations ensemble, et les deux jobs portent celle-ci mot pour mot —
# une cuisson et un rendu qui n'échantillonnent pas la même bande décrivent
# deux tournages différents, et les deux annoncent une réussite.
if [ -z "${JOB_FPS:-}" ]; then
    IFS=: read -r _kind _p1 _p2 _rest <<< "${JOB_TRAJECTORY:-}"
    case "$_kind" in
        pyrenees) JOB_FPS="${_p2:-24}" ;;
        *)        JOB_FPS=24 ;;
    esac
fi

# Une bande fournie l'emporte sur une bande générée.
#
# `JOB_TAPE_URL` sert à recuire un pack **sur son propre tracé**. Les
# générateurs évoluent — `pyrenees-tape` sortait une polyligne quand les
# premiers films ont été tournés et sort une spline aujourd'hui — donc la même
# chaîne d'arguments ne décrit plus le même vol. Pour demander si la traversée
# d'aujourd'hui s'effondre encore au-dessus de la même mer, il faut survoler
# exactement la même mer, et seul l'ancien pack sait laquelle : il enregistre
# la caméra de chaque frame. `tuile-bake --tape-from` les rejoue en bande.
#
# La bande, pas le pack : quelques centaines de kilooctets au lieu du
# gigaoctet, dans un conteneur dont le système de fichiers est de la RAM.
if [ -n "${JOB_TAPE_URL:-}" ]; then
    if ! curl -fsS -o "$tape" "$JOB_TAPE_URL"; then
        echo TAPE-DOWNLOAD-FAILED; exit 1
    fi
    echo "tape: fournie ($(du -h "$tape" | cut -f1)), trajectoire ignorée"
else
IFS=: read -r kind p1 p2 p3 p4 p5 <<< "$JOB_TRAJECTORY"
case "$kind" in
    orbit) /opt/tuile/bin/orbit-tape "$tape" \
               "${p1:-1440}" "${p2:-2.17}" "${p3:-42.52}" \
               "${p4:-8000}" "${p5:-5000}" ;;
    pyrenees) /opt/tuile/bin/pyrenees-tape "$tape" \
                  "${p1:-2}" "$JOB_FPS" "${p3:-50000}" "${p4:-0.40}" ;;
    zoom)  /opt/tuile/bin/zoom-tape "$tape" "${p1:-64}" ;;
    *) echo "TRAJECTORY-UNKNOWN: $kind"; exit 1 ;;
esac
fi
[ -s "$tape" ] || { echo TAPE-MISSING; exit 1; }

t0=$(date +%s)

# `tuile_det` reste muet.
#
# Il émet une ligne PAR TUILE VISITÉE. Mesuré : 190 Mo de journal en une minute
# de cuisson, avant d'avoir cuit une seule frame. C'est un instrument de
# diagnostic, pas un réglage de production — il s'arme par TUILE_LOG.
export TUILE_LOG="${TUILE_LOG:-info,tuile_det=warn,foyer_storage=warn}"
export TUILE_CACHE_DIR="${TUILE_CACHE_DIR:-/tmp/tuile-cache}"

# Le cache de fetch n'est pas sur un disque, il est dans la RAM du conteneur.
#
# foyer appelle son second étage « disque » et lui donne 4 Gio par défaut. Sur
# Cloud Run il n'y a pas de disque : /tmp est un tmpfs, donc ces 4 Gio sont de
# la mémoire — mais une mémoire que le processus ne voit pas, ne pèse pas et
# n'évince pas, pendant qu'elle compte dans la limite du conteneur. Autant les
# donner à l'étage que foyer borne exactement.
#
# Mesuré le 19 septembre 2026 : deux cuissons à 20 km tuées en statut 137 sans
# avoir cuit une seule frame.
export TUILE_CACHE_DISK_MB="${TUILE_CACHE_DISK_MB:-512}"
export TUILE_CACHE_MEMORY_MB="${TUILE_CACHE_MEMORY_MB:-2048}"

# Et la ligne MEMORY, toutes les quinze secondes. Sans elle un statut 137 ne
# dit rien : le conteneur disparaît avant de pouvoir se plaindre.
export TUILE_MEMORY_EVERY="${TUILE_MEMORY_EVERY:-15}"

# `--sse`, qui manquait, et qui est la moitié du contenu du pack.
#
# `JOB_SSE` arrivait jusqu'ici, se faisait afficher plus haut, et s'arrêtait
# là : la ligne de commande ne le portait pas et `tuile-bake` n'avait pas
# l'option. Tous les packs cuits jusqu'au 17 septembre 2026 le furent au défaut
# de 16, pendant que la clé R2 qui les nomme annonçait la valeur demandée. Le
# nom disait 3, le contenu valait 16, et les deux étaient cohérents entre eux.
# Les sources, quand elles sont nommées. Sans elles, `tuile-bake` garde ses
# défauts — World Terrain drapé de Bing Aerial — et c'est le cas courant.
sources=()
[ -n "${JOB_IMAGERY_ASSET:-}" ] && sources+=(--imagery "$JOB_IMAGERY_ASSET")
[ -n "${JOB_TERRAIN_ASSET:-}" ] && sources+=(--terrain "$JOB_TERRAIN_ASSET")
/opt/tuile/bin/tuile-bake --tape "$tape" --frames "$JOB_FRAMES" \
    --out "$pack" --viewport "$JOB_VIEWPORT" --sse "$JOB_SSE" \
    ${sources[@]+"${sources[@]}"} \
    | tee "$outdir/bake-out.txt"
status=${PIPESTATUS[0]}
# Le jeton a fini son travail. Rien en dessous n'en a besoin, et une variable
# qui n'existe plus ne fuit pas dans un journal, un profil ou un cœur.
unset TUILE_ION_TOKEN
if [ "$status" != 0 ] || [ ! -s "$pack" ]; then
    echo "BAKE-FAILED (status $status)"
    exit 1
fi
echo "WALL: $(($(date +%s) - t0))s pour $JOB_FRAMES"
echo "pack: $(du -h "$pack" | cut -f1)"

# La clef que `tuile-bake` imprime est celle qu'un rendu devra recevoir :
# packs/<digest de scène>/<first>-<last>.tuilepack. Elle est relayée ici pour
# qu'elle survive dans le journal du job, et non seulement dans son stdout.
grep -E '^BAKE-KEY ' "$outdir/bake-out.txt" || echo "BAKE-KEY-MISSING"

if [ -n "${JOB_PACK_PUT_URL:-}" ]; then
    if curl -fsS -T "$pack" "$JOB_PACK_PUT_URL" > /dev/null; then
        echo PACK-UP
    else
        # Un pack cuit et non déposé est une cuisson qui n'a pas eu lieu : la
        # machine s'en va avec, et personne ne peut la rejouer.
        echo PACK-UP-FAILED
        exit 1
    fi
fi

# La bande, à côté du pack, parce que c'est elle qui le définit.
#
# Elle n'était conservée nulle part : le job la fabriquait dans le conteneur,
# cuisait avec, et la jetait. Le pack, le digest et le film étaient archivés ;
# le tracé, non — alors qu'il est la seule chose dont les trois découlent.
#
# Ce que ça a coûté : pour recuire le premier film sur son propre tracé, il a
# fallu redescendre un gigaoctet de pack et en extraire les caméras une à une
# (`tuile-bake --tape-from`). Ça a marché parce que le pack enregistre la
# caméra de chaque frame — mais ça n'aurait pas dû être nécessaire, et ça ne
# marcherait pas pour un tracé dont aucun pack n'a survécu.
#
# Déposée même quand elle a été FOURNIE : un pack recuit doit porter son tracé
# comme les autres, sinon le trou se rouvre au coup d'après.
if [ -n "${JOB_TAPE_PUT_URL:-}" ]; then
    if curl -fsS -T "$tape" "$JOB_TAPE_PUT_URL" > /dev/null; then
        echo "TAPE-UP ($(du -h "$tape" | cut -f1))"
    else
        echo TAPE-UP-FAILED
    fi
fi

# Le digest de scène, à côté du pack.
#
# C'est le job qui le connaît — il couvre les réglages de traversée résolus, que
# le lanceur ne peut pas deviner — et c'est le rendu qui en a besoin, pour
# refuser un pack d'un autre globe. Déposé ICI plutôt que rapporté au lanceur :
# un rangement qui dépend qu'un processus distant reste en vie n'en est pas un.
# Mesuré le 16 septembre : le lanceur s'est arrêté entre la cuisson et le
# rangement, et deux gigaoctets valides sont restés sous une clef que personne
# ne cherche.
scene=$(sed -n 's|^BAKE-KEY packs/\([^/]*\)/.*|\1|p' "$outdir/bake-out.txt" | head -n 1)
if [ -n "${JOB_SCENE_PUT_URL:-}" ] && [ -n "$scene" ]; then
    printf '%s\n' "$scene" > "$outdir/scene.txt"
    if curl -fsS -T "$outdir/scene.txt" "$JOB_SCENE_PUT_URL" > /dev/null; then
        echo "SCENE-UP $scene"
    else
        echo "SCENE-UP-FAILED $scene"
    fi
fi
echo BAKE-DONE
