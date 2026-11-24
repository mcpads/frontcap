"""Keep CPU accounting and invalid clock IDs outside elapsed-clock virtualization."""
import json
from pathlib import Path


def check_posix_clocks(path, mode, record):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines()]
    start = [r for r in rows if r['phase'] == 'start']
    record('identity', len(start) == 1 and start[0]['mode'] == mode
           and start[0]['injected'] is (mode != 'real') and type(start[0]['pid']) is int and start[0]['pid'] > 0, start)
    cpu = [r for r in rows if r['phase'] in ('cpu-before','cpu-after')]
    expected = {(phase,clock,api) for phase in ('cpu-before','cpu-after')
                for clock in ('thread','process') for api in ('timespec','nsec')}
    record('cpu.coverage', len(cpu) == len(expected) and {(r['phase'],r['clock'],r['api']) for r in cpu} == expected, cpu)
    for r in cpu:
        # Mach converts user/system CPU components to whole microseconds separately.
        # Their sum can trail the precise combined counter by less than two microseconds.
        record('.'.join((r['phase'],r['clock'],r['api'])), r['result'] == 0 and r['errno'] == 0
               and all(type(r[k]) is int and r[k] >= 0 for k in ('before_ns','value_ns','after_ns'))
               and r['before_ns'] <= r['after_ns'] and r['before_ns'] <= r['value_ns'] < r['after_ns']+2000, r)
    budget = [r for r in rows if r['phase'] == 'cpu-budget']
    record('cpu.budget', len(budget) == 1 and budget[0]['completed'] is True
           and budget[0]['budget_ns'] == 20000000 and budget[0]['host_limit_ns'] == 2000000000
           and 0 < budget[0]['host_ns'] <= 2000000000
           and budget[0]['cpu_after_ns']-budget[0]['cpu_before_ns'] >= 20000000
           and budget[0]['native_after_ns']-budget[0]['native_before_ns'] >= 19000000, budget)
    invalid = [r for r in rows if r['phase'] == 'invalid']
    record('invalid', len(invalid) == 2 and {r['id'] for r in invalid} == {-1,2147483647}
           and all((r['result'],r['errno'],r['nsec'],r['nsec_errno']) == (-1,22,0,22) for r in invalid), invalid)
    names = ('realtime','monotonic','continuous','continuous_approx','uptime','uptime_approx')
    phases = ('elapsed-before','elapsed-after','elapsed-advanced') if mode == 'manual' else ('elapsed-before','elapsed-after')
    elapsed = [r for r in rows if r['phase'].startswith('elapsed-') and r['phase'] != 'elapsed-window']
    expected = {(phase,clock) for phase in phases for clock in names}
    covered = (len(elapsed) == len(expected) and {(r['phase'],r['clock']) for r in elapsed} == expected)
    record('elapsed.coverage', covered, elapsed)
    window = [r for r in rows if r['phase'] == 'elapsed-window']
    usable = len(window) == 1 and 100000000 <= window[0]['host_ns'] <= 3000000000
    record('elapsed.window', usable, window)
    if not covered or not usable:return
    by_key = {(r['phase'],r['clock']):r for r in elapsed}
    for clock in names:
        for api in ('timespec_ns','nsec_ns'):
            before, after = (by_key[(phase,clock)][api] for phase in phases[:2])
            valid = type(before) is int and type(after) is int and before >= 0
            if mode == 'manual':valid &= before == after and window[0]['now_ns'] == 0
            else:
                scale = 10 if mode == 'scaled' else 1
                valid &= .85*scale <= (after-before)/window[0]['host_ns'] <= 1.15*scale
            record(f'elapsed.{clock}.{api}', valid, dict(before=before,after=after,host_ns=window[0]['host_ns']))
            if mode == 'manual':
                advanced = by_key[('elapsed-advanced',clock)][api]
                record(f'advance.{clock}.{api}', abs(advanced-before-1000000000) <= 1000, dict(before=before,advanced=advanced))
    for phase in phases:
        for family in ('continuous','uptime'):
            a,b = (by_key[(phase,name)]['nsec_ns'] for name in (family,family+'_approx'))
            margin = 0 if mode == 'manual' else 20000000
            if len(start) == 1 and start[0].get('family_brackets') is True:
                row = by_key[(phase,family+'_approx')]
                before, after = row.get('exact_before_ns'), row.get('exact_after_ns')
                valid = (all(type(v) is int and v >= 0 for v in (before,b,after))
                         and before <= after and before-margin <= b <= after+margin
                         and (mode != 'manual' or before == b == after))
                record(f'{phase}.{family}_family', valid,
                       dict(exact_before=before,approximate=b,exact_after=after))
            else:
                # Historical captures did not bracket the approximate read. Keep their
                # original verdict rather than inferring a missing observation interval.
                record(f'{phase}.{family}_family', abs(a-b) <= margin, dict(exact=a,approximate=b))
