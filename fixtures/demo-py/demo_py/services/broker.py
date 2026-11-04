# 不 import handle：裸名调用命中两个同名导出 → ambiguous，不猜
def dispatch_action():
    return handle()
