#!/bin/bash
# Le test, côté pod : tout est déjà construit dans l'image.
set -uo pipefail
mark(){ echo "== $*"; }
if [ -n "${PROC_SSH_PUBKEY:-}" ]; then
    apt-get update -qq && apt-get install -y -qq openssh-server > /dev/null 2>&1
    mkdir -p /root/.ssh /run/sshd
    echo "$PROC_SSH_PUBKEY" > /root/.ssh/authorized_keys
    chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys
    /usr/sbin/sshd -p 22
fi
mark plugin
PLUGDIR=/opt/plugins/cubeProc/plugin/resources
[ -f "$PLUGDIR/plugInfo.json" ] || { echo PROC-FAIL-NOPLUGIN; sleep 3600; exit 1; }
echo "plugdir: $PLUGDIR"
PROCTYPE=TestCube
echo "proceduralType: $PROCTYPE"
cat > /tmp/proc.usda <<USDA
#usda 1.0
(defaultPrim = "P")
def Xform "P" {
    def GenerativeProcedural "proc" (prepend apiSchemas = ["HydraGenerativeProceduralAPI"]) {
        token primvars:hdGp:proceduralType = "$PROCTYPE"
    }
}
USDA
cat /tmp/proc.usda
mark render
PXR_PLUGINPATH_NAME="$PLUGDIR" HDGP_INCLUDE_DEFAULT_RESOLVER=1 \
TF_DEBUG=PLUG_REGISTRATION \
    blender -b --factory-startup -P /opt/proc/proc_driver.py 2>&1 \
    | grep -vE '^(Fra:|Saved:)' > /tmp/render-full.log; \
    grep -iE "cubeProc|HOOK|RENDERED|PIXEL|Error|hdGp" /tmp/render-full.log | tee /tmp/render.log
mark stages-composes
for m in ref direct; do
  echo "--- composed-$m (extrait proc):"
  grep -A6 -iE "proc_ref|proc_direct|GenerativeProcedural" /tmp/composed-$m.usda 2>/dev/null | head -20
done
mark verdict
VERDICT=PROC-FAIL-HOOK
if grep -q HOOK-FIRED /tmp/render.log; then
    CH=$(grep -oE 'changed=[0-9.]+' /tmp/render.log | cut -d= -f2 | tr -d '%')
    if awk -v c="${CH:-0}" 'BEGIN{exit !(c > 0.2)}'; then VERDICT=PROC-OK; else VERDICT=PROC-FAIL-COOK; fi
fi
echo "$VERDICT"
if [ "$VERDICT" = "PROC-OK" ]; then
    mark video-generative
    mkdir -p /out
    PXR_PLUGINPATH_NAME="$PLUGDIR" HDGP_INCLUDE_DEFAULT_RESOLVER=1 \
        blender -b --factory-startup -P /opt/proc/proc_video.py 2>&1 \
        | grep -vE '^(Fra:|Saved:|Append frame)' | tail -20
    if [ -s /out/proc-cube.mp4 ]; then
        ls -la /out/proc-cube.mp4
        echo GEN-VIDEO-DONE
    else
        echo GEN-VIDEO-MISSING
    fi
fi
sleep 3600
