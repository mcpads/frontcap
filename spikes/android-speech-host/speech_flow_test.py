import copy
import unittest
from speech_flow import completed_flow_ids


def completed():
    return [dict(event='boundary', identity=1, kind=kind, request_count=1,
                 correlation='single-observed-request', local_error=0,
                 observer_exception=False, limit_reached=False, sequence=i + 1)
            for i, kind in enumerate(['speak', 'onStart', 'onSuccess'])]


class FlowEvidence(unittest.TestCase):
    def test_actual_order_is_required_and_errors_are_not_completion(self):
        rows = completed()
        self.assertEqual(completed_flow_ids(rows), [1])
        self.assertEqual(completed_flow_ids(rows[1:]), [])
        self.assertEqual(completed_flow_ids([rows[1], rows[0], rows[2]]), [])
        for field, value in [('kind', 'onError'), ('local_error', 1), ('observer_exception', True),
                             ('limit_reached', True), ('sequence', 1)]:
            broken = copy.deepcopy(rows)
            broken[2][field] = value
            self.assertEqual(completed_flow_ids(broken), [], field)

    def test_reused_tokens_never_join_old_start_to_new_completion(self):
        rows = completed()
        rows[-1].update(request_count=2, correlation='reused-token')
        self.assertEqual(completed_flow_ids(rows), [])
        other = completed()
        for row in other:
            row['identity'] = 2
            row['sequence'] += 10
        self.assertEqual(completed_flow_ids(rows + other), [2])


if __name__ == '__main__':
    unittest.main()
