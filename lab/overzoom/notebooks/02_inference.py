# %% [markdown]
# # 02 — Inference: a detached job on every GPU, which stops the machine when it ends
#
# Runs on the GPU machine, and only starts the job: `python -m overzoom.batch`, detached from this notebook. The job
# fetches the zone's archives and the dataset's index from the bucket, runs a short trial then the whole dataset on
# every GPU (one process per GPU, each with a pool of CPU workers rebuilding the inputs), and **stops this notebook
# instance when it ends** — success, failure or budget spent. Closing this notebook, the browser or the laptop changes
# nothing.
#
# Outputs are lossless PNG packed in Parquet shards, each sent to the bucket as soon as it is written. Progress:
# `runs/<run>/status.json` and `runs/<run>/job.log` in the bucket. Running the job again resumes after the last shard.
#
# | | |
# |---|---|
# | samples | every tile of the dataset |
# | inputs | all 15: the 9 synthetic damages, and z18, z17, z16 as served and equalised |
# | networks | SwinIR-M classic (fidelity) · Real-ESRGAN (invention), each native at ×2, ×4, ×8 |
# | stored | `runs/<run>/shards/*.parquet` (images) and `runs/<run>/index/*.parquet` (keys, where each image is) |

# %%
import subprocess
import sys
from pathlib import Path

REMOTE = "s3://tuile-notebooks-057321054380/overzoom"
HOME = Path.home() / "SageMaker" / "overzoom"
LIB = HOME / "lib"
DATASET, RUN = "luchon-z19-v3", "luchon-v3-all"
subprocess.run(["aws", "s3", "sync", f"{REMOTE}/code/overzoom", str(LIB), "--delete", "--only-show-errors"],
               check=True)
subprocess.run([sys.executable, "-m", "pip", "install", "-q", f"{LIB}[infer]"], check=True)
subprocess.run([sys.executable, "-m", "pip", "install", "-q", "--no-deps", "--force-reinstall", str(LIB)], check=True)

# %% [markdown]
# ## The job

# %%
out = open(HOME / "job.out", "a")
job = subprocess.Popen([sys.executable, "-m", "overzoom.batch", "--remote", REMOTE, "--dataset", DATASET,
                        "--run", RUN, "--trial", "32", "--budget", "4", "--stop"],
                       stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
print(f"job {job.pid} started; follow it with:  aws s3 cp {REMOTE}/runs/{RUN}/status.json -")

# %% [markdown]
# ## Where it is (run this cell again at will)

# %%
print((HOME / "jobs" / RUN / "status.json").read_text())
print("".join(open(HOME / "jobs" / RUN / "job.log").readlines()[-10:]))
