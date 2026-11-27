"""perf-chart.py - turn one M1LK test run into an SVG chart (no game imagery, numbers only).

Panels: frames per second, draws drawn vs dropped per frame, and process memory (private GB),
over minutes since start. Inputs: the runtime log (fps lines from victory_gpu::host) and
optionally probe.ps1's samples.csv. Standard library only.

  python perf-chart.py --log run.log [--samples samples.csv] --title "build x" --out chart.svg
"""
import argparse, csv, re, datetime as dt

FPS = re.compile(r'^(\S+Z)\s+INFO victory_gpu::host: ([\d.]+) fps, ([\d.]+) ms/frame.*?, ([\d.]+) draws/frame, dropped ([\d.]+)')

def ts(s):
    return dt.datetime.fromisoformat(s.replace('Z', '+00:00'))

def read_log(path):
    pts, t0 = [], None
    with open(path, encoding='utf-8', errors='replace') as f:
        for line in f:
            m = FPS.match(line)
            if not m:
                if t0 is None and line[:4].isdigit():
                    try: t0 = ts(line.split()[0])
                    except ValueError: pass
                continue
            t = ts(m.group(1)); t0 = t0 or t
            pts.append(((t - t0).total_seconds() / 60, float(m.group(2)), float(m.group(4)), float(m.group(5))))
    return pts

def read_samples(path):
    with open(path, encoding='utf-8-sig') as f:
        return [(float(r['t']) / 60, float(r['private_mb']) / 1024, float(r['free_ram_mb']) / 1024) for r in csv.DictReader(f)]

def panel(y0, h, w, series, xmax, label, unit, limit=None):
    ymax = max([v for s in series for _, v in s[0]] + [limit or 0, 1]) * 1.1
    out = [f'<text x="10" y="{y0 + 14}" class="l">{label}</text>',
           f'<rect x="60" y="{y0 + 20}" width="{w - 80}" height="{h - 40}" class="box"/>']
    for i in range(5):
        v = ymax * i / 4; y = y0 + h - 20 - (h - 40) * i / 4
        out.append(f'<text x="55" y="{y + 4:.0f}" class="a" text-anchor="end">{v:.0f}{unit}</text>')
    if limit:
        y = y0 + h - 20 - (h - 40) * limit / ymax
        out.append(f'<line x1="60" x2="{w - 20}" y1="{y:.1f}" y2="{y:.1f}" class="lim"/><text x="{w - 22}" y="{y - 4:.0f}" class="a" text-anchor="end">goal {limit:g}{unit}</text>')
    for pts, cls, name in series:
        if not pts: continue
        d = ' '.join(f'{60 + (w - 80) * x / xmax:.1f},{y0 + h - 20 - (h - 40) * v / ymax:.1f}' for x, v in pts)
        out.append(f'<polyline points="{d}" class="{cls}"/>')
    lx = 70
    for _, cls, name in series:
        out.append(f'<line x1="{lx}" x2="{lx + 16}" y1="{y0 + 32}" y2="{y0 + 32}" class="{cls}"/><text x="{lx + 20}" y="{y0 + 36}" class="a">{name}</text>'); lx += 150
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--log', required=True); ap.add_argument('--samples'); ap.add_argument('--title', default='M1LK test run')
    ap.add_argument('--out', required=True); ap.add_argument('--ram-goal', type=float, default=13)
    a = ap.parse_args()
    log = read_log(a.log); smp = read_samples(a.samples) if a.samples else []
    xmax = max([p[0] for p in log] + [p[0] for p in smp] + [1])
    w, h = 900, 200
    parts = panel(30, h, w, [([(x, f) for x, f, _, _ in log], 's1', 'fps')], xmax, 'Frames per second', '', 30)
    parts += panel(30 + h, h, w, [([(x, d) for x, _, d, _ in log], 's2', 'draws drawn'), ([(x, r) for x, _, _, r in log], 's3', 'draws dropped')], xmax, 'Draws per frame', '')
    if smp:
        parts += panel(30 + 2 * h, h, w, [([(x, p) for x, p, _ in smp], 's3', 'process RAM'), ([(x, f) for x, _, f in smp], 's2', 'free system RAM')], xmax, 'Memory (GB)', '', a.ram_goal)
    n = 3 if smp else 2
    peak = f', peak RAM {max(p for _, p, _ in smp):.1f} GB' if smp else ''
    best = f'median fps {sorted(f for _, f, _, _ in log)[len(log) // 2]:.1f}' if log else 'no frames'
    H = 60 + n * h
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{H}" viewBox="0 0 {w} {H}">',
           '<style>text{font:12px sans-serif;fill:#333}.l{font-weight:bold}.a{font-size:11px;fill:#666}.box{fill:#fafafa;stroke:#ccc}'
           'polyline,line{fill:none;stroke-width:1.6}.s1{stroke:#2a7ab9}.s2{stroke:#3a9a4a}.s3{stroke:#c0392b}.lim{stroke:#999;stroke-dasharray:4 3}</style>',
           f'<rect width="{w}" height="{H}" fill="#fff"/><text x="10" y="18" class="l">{a.title}: {xmax:.1f} min, {best}{peak}</text>']
    svg += parts
    svg.append(f'<text x="{w // 2}" y="{H - 6}" class="a" text-anchor="middle">minutes since start</text></svg>')
    open(a.out, 'w', encoding='utf-8').write('\n'.join(svg))
    print(a.out, len(log), 'fps samples', len(smp), 'memory samples')

if __name__ == '__main__':
    main()
