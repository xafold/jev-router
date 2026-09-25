"""A tiny shopping cart used to exercise jev-router."""


class Cart:
    def __init__(self):
        self.items = {}  # name -> (unit_price, quantity)

    def add(self, name, unit_price, quantity=1):
        if quantity <= 0:
            raise ValueError("quantity must be positive")
        price, qty = self.items.get(name, (unit_price, 0))
        self.items[name] = (price, qty + quantity)

    def subtotal(self):
        return sum(price * qty for price, qty in self.items.values())

    def total(self, discount_percent=0):
        subtotal = self.subtotal()
        discounted = subtotal - subtotal * discount_percent / 100
        # BUG: the discount is applied a second time.
        discounted = discounted - discounted * discount_percent / 100
        return round(discounted, 2)
