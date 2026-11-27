"""breakdown.py - rank where an M1LK run spends frame time and memory (measure, then cut the top items).

Reads a runtime log (victory_gpu::host fps lines + copystat memory-category lines) and prints a markdown report:
  1. frame time split (median and worst quarter): preparing (copies, textures), GPU, copy-back, heap passes,
     and "unexplained" (time not in any measured stage: usually paging stalls or CPU waits)
  2. draws drawn / dropped (no pipeline, indirect), textures waiting
  3. memory buckets at the peak private total, largest first
Compare two runs: python breakdown.py A.log B.log   (prints both and the change).
"""
import re, sys, statistics as st

FPS = re.compile(r'INFO victory_gpu::host: ([\d.]+) fps, ([\d.]+) ms/frame \(([\d.]+) preparing: copies ([\d.]+) ms ([\d.]+) MB, textures ([\d.]+) ms ([\d.]+) MB; ([\d.]+) on the GPU; ([\d.]+) copying back ([\d.]+) MB\), ([\d.]+) draws/frame, dropped ([\d.]+) without a host pipeline \((\d+) being made\) and ([\d.]+) indirect; heap buffers [\d.]+ passes ([\d.]+) ms')
WAIT = re.compile(r'(\d+) waiting')
MEM = re.compile(r'copystat: memory by category[^:]*:(.*)$')
ITEM = re.compile(r'\s*([^,]+?) (\d+) MB')

def parse(path):
    frames, mem = [], []
    for line in open(path, encoding='utf-8', errors='replace'):
        m = FPS.search(line)
        if m:
            v = [float(x) for x in m.groups()]
            w = WAIT.search(line[m.end():])
            frames.append(dict(fps=v[0], ms=v[1], prep=v[2], copies=v[3], copies_mb=v[4], tex=v[5], tex_mb=v[6], gpu=v[7],
                               back=v[8], back_mb=v[9], draws=v[10], dropped=v[11], making=v[12], indirect=v[13], passes=v[14],
                               waiting=float(w.group(1)) if w else 0))
            continue
        m = MEM.search(line)
        if m:
            mem.append({k.strip(): int(n) for k, n in ITEM.findall(m.group(1))})
    return frames, mem

def summarize(frames, mem):
    out = {}
    if frames:
        worst = sorted(frames, key=lambda f: f['ms'])[-max(1, len(frames) // 4):]
        for name, sel in (('median', frames), ('worst 25%', worst)):
            g = lambda k: st.median(f[k] for f in sel)
            ms = g('ms'); known = g('prep') + g('gpu') + g('back') + g('passes')
            out[name] = {'fps': g('fps'), 'ms/frame': ms, 'preparing': g('prep'), '  copies': g('copies'), '  textures': g('tex'),
                         'GPU': g('gpu'), 'copy-back': g('back'), 'heap passes': g('passes'), 'unexplained (stalls/paging)': max(0, ms - known),
                         'draws': g('draws'), 'dropped: no pipeline': g('dropped'), 'dropped: indirect': g('indirect'), 'textures waiting': g('waiting')}
    if mem:
        peak = max(mem, key=lambda d: d.get('private total', 0))
        out['memory'] = dict(sorted(peak.items(), key=lambda kv: -kv[1]))
    return out

def report(path):
    s = summarize(*parse(path))
    print(f'## {path}\n')
    for name in ('median', 'worst 25%'):
        if name in s:
            print(f'### Frame ({name})\n\n| item | value |\n|---|---|')
            for k, v in s[name].items():
                print(f'| {k} | {v:,.1f} |')
            print()
    if 'memory' in s:
        print('### Memory at peak (MB, overlapping buckets, largest first)\n\n| bucket | MB |\n|---|---|')
        for k, v in list(s['memory'].items())[:16]:
            print(f'| {k} | {v:,} |')
        print()
    return s

if __name__ == '__main__':
    runs = [report(p) for p in sys.argv[1:]]
    if len(runs) == 2 and all('median' in r for r in runs):
        print('### Change B - A (median)\n\n| item | A | B | change |\n|---|---|---|---|')
        for k in runs[0]['median']:
            a, b = runs[0]['median'][k], runs[1]['median'][k]
            print(f'| {k} | {a:,.1f} | {b:,.1f} | {b - a:+,.1f} |')
