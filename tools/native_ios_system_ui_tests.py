import copy
import json
from pathlib import Path
import unittest

from native_ios_generic_ui import UIKitUIRequest, validate_generic_result
from native_ios_system_ui import target, validate_catalog


class SystemUiTests(unittest.TestCase):
    def test_system_evidence_is_bounded_and_does_not_claim_app_clock_state(self):
        catalog = json.loads((Path(__file__).resolve().parents[1]/
            'crates/core/tests/fixtures/ios-system-ui.json').read_text())
        observation = catalog['observation']
        reference = catalog['alerts'][0]['buttons'][1]['reference']
        self.assertEqual(target(catalog, observation, reference)['label'], 'Open')
        for change in ('unstable', 'disabled', 'unhittable', 'foreign', 'oversize', 'duplicate'):
            data = copy.deepcopy(catalog)
            button = data['alerts'][0]['buttons'][1]
            if change == 'unstable': data['stable'] = False
            elif change == 'disabled': button.update(enabled=False, hittable=False)
            elif change == 'unhittable': button['hittable'] = False
            elif change == 'foreign': data['bundle'] = 'dev.other.app'
            elif change == 'oversize': data['alerts'][0]['texts'] = ['x'*1025]
            else: data['alerts'][0]['buttons'].append(copy.deepcopy(button))
            with self.subTest(change=change), self.assertRaises(ValueError):
                target(data, observation, reference)
        overlapping = copy.deepcopy(catalog)
        overlapping['alerts'].append(copy.deepcopy(catalog['alerts'][0]))
        for i, button in enumerate(b for a in overlapping['alerts'] for b in a['buttons']):
            button.update(reference=observation+'-'+str(i), unique=False, hittable=False)
        validate_catalog(overlapping)
        with self.assertRaises(ValueError): target(overlapping, observation, reference)
        empty = dict(catalog, alerts=[])
        validate_catalog(empty)
        with self.assertRaises(ValueError): target(empty, observation, reference)
        request = UIKitUIRequest('abcd0000-0000-0000-0000-000000000002', 2, 'observe_system_alerts', {})
        result = dict(state='system_observed', catalog=catalog, artifacts=[
            dict(path='observations/'+observation+'/'+name, bytes=10, sha256='a'*64)
            for name in ('screen.png', 'hierarchy.txt')])
        validate_generic_result(result, request)
        with self.assertRaises(ValueError): validate_generic_result(dict(result, now_ns=0), request)
        tap = UIKitUIRequest(request.generation, 3, 'tap_system_button',
                             dict(reference=reference, observation=observation))
        validate_generic_result(dict(input='xctest_system_tap', **tap.arguments), tap)
        validate_generic_result(dict(input='not_applied', reason='observation_changed', **tap.arguments), tap)
        with self.assertRaises(ValueError):
            validate_generic_result(dict(input='xctest_system_tap', reference=observation+'-0', observation=observation), tap)


if __name__ == '__main__': unittest.main()
