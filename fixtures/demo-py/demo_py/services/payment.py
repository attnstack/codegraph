from demo_py.utils.logger import log
from demo_py.utils.money import format


def charge(amount):
    log(f"charging {amount}")
    return format(amount)
