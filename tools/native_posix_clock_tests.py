"""Reject virtual CPU counters, forged clock success and elapsed-clock escape."""
import json
from pathlib import Path
import tempfile
import unittest

from native_posix_clock_checks import check_posix_clocks


def clock_evidence(mode):
    rows = [dict(phase='start',mode=mode,pid=123,injected=mode != 'real')]
    for phase in ('cpu-before','cpu-after'):
        for clock in ('process','thread'):
            for api in ('timespec','nsec'):
                rows.append(dict(phase=phase,clock=clock,api=api,before_ns=1000000,value_ns=1000500,
                                 after_ns=1001000,result=0,errno=0))
    rows.append(dict(phase='cpu-budget',completed=True,budget_ns=20000000,host_limit_ns=2000000000,
                     host_ns=22000000,cpu_before_ns=1000000,cpu_after_ns=21000000,
                     native_before_ns=1000000,native_after_ns=21000000))
    phases = ('elapsed-before','elapsed-after','elapsed-advanced') if mode == 'manual' else ('elapsed-before','elapsed-after')
    scale = {'real':1,'scaled':10,'manual':0}[mode]
    for phase in phases:
        value = 5000000000 + (1000000000 if phase == 'elapsed-advanced' else 120000000*scale if phase == 'elapsed-after' else 0)
        for clock in ('realtime','monotonic','continuous','continuous_approx','uptime','uptime_approx'):
            rows.append(dict(phase=phase,clock=clock,timespec_ns=value,nsec_ns=value))
    rows.append(dict(phase='elapsed-window',host_ns=120000000,now_ns=0))
    rows.extend(dict(phase='invalid',id=number,result=-1,errno=22,nsec=0,nsec_errno=22) for number in (-1,2147483647))
    return rows


class PosixClockTests(unittest.TestCase):
    def verify(self, rows, mode='manual'):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)/'clocks.jsonl';path.write_text('\n'.join(json.dumps(r) for r in rows))
            checks = {};check_posix_clocks(path,mode,lambda name,passed,observed:checks.update({name:passed}))
        return checks

    def test_native_cpu_accounting_and_elapsed_domains(self):
        for mode in ('real','scaled','manual'):
            with self.subTest(mode=mode):
                checks = self.verify(clock_evidence(mode),mode)
                self.assertTrue(all(checks.values()),checks)

    def bracketed(self, mode):
        rows = clock_evidence(mode)
        rows[0]['family_brackets'] = True
        for row in rows:
            if row.get('clock', '').endswith('_approx'):
                value = row['nsec_ns'] + (60000000 if mode == 'scaled' else 0)
                row.update(nsec_ns=value, exact_before_ns=value, exact_after_ns=value)
        return rows

    def test_family_comparison_uses_the_actual_read_interval_not_an_earlier_sample(self):
        rows = self.bracketed('scaled')
        self.assertTrue(all(self.verify(rows, 'scaled').values()))
        rows[0].pop('family_brackets')
        self.assertFalse(self.verify(rows, 'scaled')['elapsed-after.continuous_family'])

    def test_invalid_brackets_and_out_of_interval_clocks_are_rejected(self):
        for mutation in ('missing', 'reversed', 'outside', 'negative', 'boolean'):
            with self.subTest(mutation=mutation):
                rows = self.bracketed('scaled')
                row = next(r for r in rows if r.get('clock')=='continuous_approx')
                if mutation=='missing':row.pop('exact_after_ns')
                elif mutation=='reversed':row['exact_before_ns']+=1
                elif mutation=='outside':row['nsec_ns']+=20000001
                elif mutation=='negative':row['exact_before_ns']=-1
                else:row['exact_before_ns']=True
                self.assertFalse(self.verify(rows, 'scaled')['elapsed-before.continuous_family'])
        rows = self.bracketed('manual')
        row = next(r for r in rows if r.get('clock')=='continuous_approx')
        row['exact_after_ns']+=1
        self.assertFalse(self.verify(rows)['elapsed-before.continuous_family'])

    def test_separately_quantized_cpu_components_have_a_two_microsecond_bound(self):
        rows=clock_evidence('manual');rows[1]['value_ns']=rows[1]['after_ns']+1999
        self.assertTrue(self.verify(rows)['cpu-before.process.timespec'])
        rows[1]['value_ns']+=1
        self.assertFalse(self.verify(rows)['cpu-before.process.timespec'])

    def test_cpu_alias_and_false_budget_completion_are_rejected(self):
        for change, name in (
                (lambda rows:rows[1].update(value_ns=5000000000),'cpu-before.process.timespec'),
                (lambda rows:next(r for r in rows if r['phase']=='cpu-budget').update(completed=False),'cpu.budget'),
                (lambda rows:next(r for r in rows if r['phase']=='cpu-budget').update(native_after_ns=3000000),'cpu.budget'),
                (lambda rows:rows.pop(1),'cpu.coverage')):
            with self.subTest(name=name):
                rows=clock_evidence('manual');change(rows);self.assertFalse(self.verify(rows)[name])

    def test_invalid_success_clock_drift_and_wrong_approximate_domain_are_rejected(self):
        mutations = [
            ('invalid',lambda rows:rows[-1].update(result=0,errno=0,nsec=5000000000,nsec_errno=0)),
            ('elapsed.uptime.nsec_ns',lambda rows:next(r for r in rows if (r['phase'],r.get('clock'))==('elapsed-after','uptime')).update(nsec_ns=5000000001)),
            ('advance.monotonic.timespec_ns',lambda rows:next(r for r in rows if (r['phase'],r.get('clock'))==('elapsed-advanced','monotonic')).update(timespec_ns=7000000000)),
            ('elapsed-before.continuous_family',lambda rows:next(r for r in rows if (r['phase'],r.get('clock'))==('elapsed-before','continuous_approx')).update(nsec_ns=4000000000)),
        ]
        for name,change in mutations:
            with self.subTest(name=name):
                rows=clock_evidence('manual');change(rows);self.assertFalse(self.verify(rows)[name])


if __name__ == '__main__':unittest.main()
