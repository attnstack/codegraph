# 下划线开头 = python 私有约定：无人调用的私有函数会被 dead-code 命中
def _orphan_helper():
    return 42
