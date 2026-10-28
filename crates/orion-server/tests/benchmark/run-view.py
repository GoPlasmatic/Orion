#!/usr/bin/env python3
"""Measure v4 run-view scaling with the real Community Wasm fixture.

Each case uses a fresh process for peak RSS; three measured engine runs follow
one warm-up. Input construction, compilation and validation are not timed.
"""
import argparse
import hashlib
import itertools
import json
import os
import platform
from pathlib import Path
import re
import statistics
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[4]


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=['dev', 'release', 'profiling'], default='dev')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--output', type=Path, default=ROOT / 'target/run-view-results.json')
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error('--rounds must be positive')
    subprocess.run(['cargo', 'build', '--locked', '-p', 'orion-plugin-host',
                    '--example', 'run_view', '--profile', args.profile], cwd=ROOT, check=True)
    # Honor Cargo target-directory configuration instead of assuming target/.
    metadata = json.loads(command('cargo', 'metadata', '--no-deps', '--format-version', '1'))
    binary = Path(metadata['target_directory']) / ('debug' if args.profile == 'dev' else args.profile) / 'examples/run_view'
    sources = [ROOT / 'crates/orion-plugin-host/examples/run_view.rs',
               ROOT / 'crates/orion-plugin-host/examples/run_view_support/allocation.rs',
               ROOT / 'Cargo.lock', Path(__file__)]
    result = {
        'commit': command('git', 'rev-parse', 'HEAD'),
        'dirty': bool(command('git', 'status', '--porcelain', '--untracked-files=no')),
        'source_sha256': {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sources},
        'toolchain': command('rustc', '-Vv'), 'platform': platform.platform(),
        'machine': platform.machine(), 'logical_cpus': os.cpu_count(),
        'profile': args.profile, 'rounds': args.rounds, 'cases': [],
    }
    if sys.platform == 'darwin':
        result['cpu'] = command('sysctl', '-n', 'machdep.cpu.brand_string')
        result['memory_bytes'] = int(command('sysctl', '-n', 'hw.memsize'))
    for kind, steps, nodes, trace in itertools.product(['loop', 'fan'], [10, 1000], [0, 1024, 16384], [0, 1]):
        for concurrency in ([1] if kind == 'loop' else [1, 8]):
            invocation = [str(binary), kind, str(nodes), str(steps), str(concurrency), str(args.rounds), str(trace)]
            timer = ['/usr/bin/time', '-l'] if sys.platform == 'darwin' else ['/usr/bin/time', '-v']
            ran = subprocess.run(timer + invocation, cwd=ROOT, text=True, capture_output=True, check=True)
            case = json.loads(ran.stdout)
            if sys.platform == 'darwin':
                rss = re.search(r'(\d+)\s+maximum resident set size', ran.stderr)
                multiplier = 1
            else:
                rss = re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)', ran.stderr)
                multiplier = 1024
            if not rss:
                raise RuntimeError(f'No peak RSS in time output: {ran.stderr}')
            case['process_peak_rss_bytes'] = int(rss[1]) * multiplier
            case['median'] = {key: statistics.median(s[key] for s in case['samples'])
                              for key in ['elapsed_ns', 'allocations', 'allocated_bytes']}
            result['cases'].append(case)
            print(f'{kind} steps={steps} nodes={nodes} trace={trace} concurrency={concurrency}: {case["median"]}', file=sys.stderr)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(args.output)


if __name__ == '__main__':
    main()
