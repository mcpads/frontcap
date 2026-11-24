import unittest
from native_android_registration_states import account, analyze


def evidence():
    owner = {'identity': {'pid': 42, 'generation': 'app'}, 'controller_id': 'controller'}
    events = [{'phase': 'registration_accepted', 'pid': 42, 'generation': 'app',
               'request': {'generation': 'app', 'controller_id': 'controller', 'request_id': 1}}]
    spans = [{'name': 'FrontcapInput:app:controller:1', 'ts': 100, 'dur': 100,
              'pid': 42, 'tid': 42, 'utid': 7}]
    states = [{'utid': 7, 'ts': 80, 'dur': 50, 'state': 'Running'},
              {'utid': 7, 'ts': 130, 'dur': 40, 'state': 'R'},
              {'utid': 7, 'ts': 170, 'dur': 50, 'state': 'S'}]
    return owner, events, spans, states


class StateTests(unittest.TestCase):
    def test_clip_to_identity_bound_span_without_charging_other_threads(self):
        owner, events, spans, states = evidence()
        states.append({'utid': 8, 'ts': 100, 'dur': 100, 'state': 'D'})
        r = analyze(owner, events, spans, list(reversed(states)))['requests'][0]['accounting']
        self.assertEqual(r, {'status': 'complete_coverage', 'duration_ns': 100,
                             'states_ns': {'Running': 30, 'R': 40, 'S': 30},
                             'gaps_ns': [], 'covered_ns': 100})

    def test_gaps_are_explicit_and_overlap_is_not_double_counted(self):
        _, _, spans, states = evidence()
        r = account(spans[0], [states[0], states[2]])
        self.assertEqual(r['status'], 'incomplete_coverage')
        self.assertEqual(r['gaps_ns'], [[130, 170]])
        self.assertEqual(r['covered_ns'], 60)
        self.assertEqual(account(spans[0], states + [states[1]])['status'], 'overlapping_thread_states')

    def test_unclosed_or_unknown_intervals_are_not_measured_as_zero(self):
        for kind in ('span', 'state', 'unknown'):
            _, _, spans, states = evidence()
            if kind == 'span': spans[0]['dur'] = -1
            elif kind == 'state': states[1]['dur'] = -1
            else: states[1]['state'] = '[NULL]'
            status = {'span': 'unclosed_span', 'state': 'unclosed_thread_state', 'unknown': 'unknown_thread_state'}[kind]
            self.assertEqual(account(spans[0], states), {'status': status})

    def test_missing_and_duplicate_labels_stay_unresolved(self):
        owner, events, spans, states = evidence()
        for candidates, status in (([], 'missing_span'), (spans * 2, 'ambiguous_span')):
            self.assertEqual(analyze(owner, events, candidates, states)['requests'][0]['accounting'], {'status': status})

    def test_foreign_owner_thread_or_duplicate_request_is_rejected(self):
        for kind in ('owner', 'thread', 'duplicate'):
            owner, events, spans, states = evidence()
            if kind == 'owner': events[0]['request']['controller_id'] = 'other'
            elif kind == 'thread': spans[0]['tid'] = 43
            else: events *= 2
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                analyze(owner, events, spans, states)


if __name__ == '__main__':
    unittest.main()
