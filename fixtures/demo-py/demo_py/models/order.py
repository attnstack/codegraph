class Order:
    def __init__(self, order_id, amount):
        self.order_id = order_id
        self.amount = amount

    def total(self):
        return self.amount
