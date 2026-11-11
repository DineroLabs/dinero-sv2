import unittest
from orchard_job_ready import wait_for_orchard_job


class RpcError(RuntimeError):
    def __init__(self, code=-32603, message='Selected Orchard mining construction unavailable'):
        super().__init__(message)
        self.code, self.rpc_message = code, message


class Readiness(unittest.TestCase):
    def setUp(self):
        self.now = 0
        self.calls = []

    def sleep(self, seconds):
        self.assertGreater(seconds, 0)
        self.assertLessEqual(seconds, .25)
        self.now += seconds

    def run_rpc(self, callback, seconds=1):
        def rpc(method, params):
            self.calls.append((method, params))
            return callback()
        return wait_for_orchard_job(rpc, 'exact-payout', seconds,
                                    clock=lambda: self.now, sleep=self.sleep)

    def test_waits_for_real_success_without_changing_job(self):
        job = {'height': 102, 'profile': 'orchard-v1', 'body': 'unchanged'}
        def reply():
            if len(self.calls) < 3:
                raise RpcError()
            return job
        self.assertIs(self.run_rpc(reply), job)
        self.assertEqual(self.now, .5)
        self.assertEqual(self.calls, [('mining.getjob', {'address': 'exact-payout'})] * 3)

    def test_permanent_errors_fail_immediately(self):
        for error in [RpcError(-1), RpcError(message='different refusal'),
                      RuntimeError('Selected Orchard mining construction unavailable'),
                      RpcError(-32603.0), ConnectionError('transport failed')]:
            self.calls = []
            def reply():
                raise error
            with self.assertRaises(type(error)) as raised:
                self.run_rpc(reply)
            self.assertIs(raised.exception, error)
            self.assertEqual(len(self.calls), 1)
            self.assertEqual(self.now, 0)

    def test_never_ready_fails_at_deadline(self):
        def reply():
            raise RpcError()
        with self.assertRaises(TimeoutError):
            self.run_rpc(reply, .5)
        self.assertEqual(self.now, .5)
        self.assertEqual(len(self.calls), 3)

    def test_success_is_not_sanitized_or_retried(self):
        # The existing qualification assertions must reject a malformed job.
        self.assertIsNone(self.run_rpc(lambda: None))
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(self.now, 0)


if __name__ == '__main__':
    unittest.main()
