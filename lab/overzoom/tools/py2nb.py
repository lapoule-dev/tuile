"""Convert a percent-format Python script (# %% cells) to a Jupyter notebook without outputs."""
import json, re, sys

src, dst = sys.argv[1], sys.argv[2]
s = open(src).read()
cells = []
for block in re.split(r'^# %%', s, flags=re.M)[1:]:
    head, _, body = block.partition('\n')
    body = body.strip('\n')
    if head.strip().startswith('[markdown]'):
        text = '\n'.join(l[2:] if l.startswith('# ') else l.lstrip('#') for l in body.split('\n'))
        cells.append({'cell_type': 'markdown', 'metadata': {}, 'source': text.splitlines(True)})
    else:
        cells.append({'cell_type': 'code', 'metadata': {}, 'execution_count': None, 'outputs': [],
                      'source': body.splitlines(True)})
nb = {'cells': cells,
      'metadata': {'accelerator': 'GPU', 'colab': {'provenance': []},
                   'kernelspec': {'name': 'python3', 'display_name': 'Python 3'},
                   'language_info': {'name': 'python'}},
      'nbformat': 4, 'nbformat_minor': 5}
with open(dst, 'w') as f:
    json.dump(nb, f, indent=1, ensure_ascii=False)
    f.write('\n')
print(len(cells), 'cells ->', dst)
