"""A small untyped Python program used to test `fpython convert`."""


class Item:
    def __init__(self, name, price, qty=0):
        self.name = name
        self.price = price
        self.qty = qty
        self.tags = []

    def total(self):
        return self.price * self.qty

    def add_tag(self, tag):
        self.tags.append(tag)

    def describe(self):
        return "%s x%d @ %.2f" % (self.name, self.qty, self.price)


class Inventory:
    def __init__(self):
        self.items = {}

    def add(self, item):
        self.items[item.name] = item

    def value(self):
        total = 0.0
        for item in self.items.values():
            total += item.total()
        return total

    def cheapest(self):
        best = None
        for item in self.items.values():
            if best is None or item.price < best.price:
                best = item
        return best

    def names(self):
        return sorted(self.items.keys())


def word_counts(text):
    counts = {}
    for word in text.lower().split():
        counts[word] = counts.get(word, 0) + 1
    return counts


def fib(n):
    a, b = 0, 1
    seq = []
    for _ in range(n):
        seq.append(a)
        a, b = b, a + b
    return seq


def main():
    inv = Inventory()
    apple = Item("apple", 0.5, 10)
    apple.add_tag("fruit")
    inv.add(apple)
    inv.add(Item("pear", 0.75, 4))
    inv.add(Item("melon", 3.0, 1))
    print("value: {:.2f}".format(inv.value()))
    print("names:", inv.names())
    cheap = inv.cheapest()
    if cheap is not None:
        print("cheapest:", cheap.describe())
    wc = word_counts("the cat and the hat and the bat")
    print(sorted(wc.items()))
    print(fib(10))


main()
