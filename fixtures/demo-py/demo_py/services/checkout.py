# 命名空间导入：payment.charge(...) 通过子模块绑定解析
from demo_py.services import payment
from demo_py.models.order import Order


class Checkout:
    def create_order(self, order_id, amount):
        order = Order(order_id, amount)
        return payment.charge(order.total())

    def validate(self):
        return True

    def process(self, callback):
        callback()
