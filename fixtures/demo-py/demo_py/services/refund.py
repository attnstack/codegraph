from .checkout import Checkout
from .payment import charge


class RefundService(Checkout):
    def validate(self):
        return False

    def process_refund(self, amount):
        self.process(lambda: None)
        return charge(amount)
