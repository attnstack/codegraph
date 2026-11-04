# 与 money 相互引用（import 环的另一条边）
from demo_py.utils.money import format


def log(message):
    print(message)


def log_and_format(amount):
    return format(amount)
