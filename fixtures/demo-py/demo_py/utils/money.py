# 与 logger 相互引用，形成 import 环（cycles 查询的演示素材）
from demo_py.utils.logger import log


def format(amount):
    log(f"formatting {amount}")
    return f"${round(amount, 2)}"
