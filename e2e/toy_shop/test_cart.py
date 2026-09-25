import unittest

from cart import Cart


class CartTest(unittest.TestCase):
    def test_subtotal(self):
        cart = Cart()
        cart.add("apple", 0.5, 4)
        cart.add("bread", 2.25)
        self.assertEqual(cart.subtotal(), 4.25)

    def test_discount(self):
        cart = Cart()
        cart.add("book", 20.0, 2)
        self.assertEqual(cart.total(discount_percent=10), 36.0)

    def test_rejects_zero_quantity(self):
        with self.assertRaises(ValueError):
            Cart().add("pen", 1.0, 0)


if __name__ == "__main__":
    unittest.main()
