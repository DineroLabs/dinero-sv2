"""Wait only for Core's explicit temporary Orchard construction response."""
import time


def wait_for_orchard_job(rpc, address, seconds=60, *, clock=time.monotonic, sleep=time.sleep):
    if seconds <= 0:
        raise ValueError('A positive readiness deadline is required')
    deadline = clock() + seconds
    while True:
        try:
            # The caller still checks the exact parent, height and typed body.
            return rpc('mining.getjob', {'address': address})
        except RuntimeError as error:
            if (type(getattr(error, 'code', None)) is not int or error.code != -32603 or
                    getattr(error, 'rpc_message', None) != 'Selected Orchard mining construction unavailable'):
                raise
            remaining = deadline - clock()
            if remaining <= 0:
                raise TimeoutError('Orchard mining construction did not become ready before the deadline') from error
            sleep(min(0.25, remaining))
