"""A printed business marker alone must not pass an instrumentation execution."""
import unittest
from android_instrumentation_probe import baseline_result


class InstrumentationResultTest(unittest.TestCase):
    def test_requires_one_proof_and_exact_success_result_code(self):
        proof = 'INSTRUMENTATION_RESULT: stream=CONSUMER_BASELINE {"items":41}\n'
        self.assertEqual(baseline_result(proof+'INSTRUMENTATION_CODE: -1\n', 'CONSUMER_BASELINE'), {'items': 41})
        for output in [proof, proof+'INSTRUMENTATION_CODE: 0\n', proof+'INSTRUMENTATION_CODE: -10\n',
                       proof+proof+'INSTRUMENTATION_CODE: -1\n',
                       'CONSUMER_BASELINE {broken}\nINSTRUMENTATION_CODE: -1\n']:
            with self.subTest(output=output):
                self.assertIsNone(baseline_result(output, 'CONSUMER_BASELINE'))


if __name__ == '__main__':
    unittest.main()
